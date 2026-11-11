//! The ticket module's dashboard state and operation bridge.

use std::fmt::Display;
use std::time::Duration;

use closure_core::action::{Action, MentionPolicy};
use closure_core::event::ChannelId;
use closure_core::module::{
    DashboardActionError, DashboardActionRequest, DashboardActionResult, DashboardScope,
    EventContext,
};
use closure_core::permission::{Actor, PermissionKey};
use closure_core::storage::{Expect, StorageError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    Args, CloseRequest, CloseRequestExpiry, Config, HistoryStatus, ReplySendAttempt, Ticket,
    blacklist, cancel_close_request_timeout, close_request_expiry, finish, history_key,
    history_staff_roles, knowledge, label_key, may_close, post_close_request_message,
    refresh_ticket_form_answer_ttl, reply_send_attempt_key, split_reply, teams_survey, ticket_key,
    ticket_ttl, valid_reply_content, watch_close_request_timeout,
};

const RECEIPT_PREFIX: &str = "ticket-dashboard-action:";
const RECEIPT_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const RECEIPT_LEASE_SECONDS: i64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum ReceiptState {
    Pending { lease_until: i64, step: ReceiptStep },
    Complete { value: Value, revision: i64 },
    Uncertain,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
enum ReceiptStep {
    Ready,
    RequestMessage,
    ReplySend { attempt_id: i64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Receipt {
    actor: u64,
    scope: String,
    action: String,
    data: Value,
    expected_revision: i64,
    ticket_revision: Option<i64>,
    ticket_number: Option<i64>,
    close_request: Option<CloseRequest>,
    state: ReceiptState,
}

enum Start {
    Replay(DashboardActionResult),
    Claimed {
        key: String,
        receipt: Receipt,
        revision: i64,
        recovering: bool,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetData {
    target: blacklist::Target,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReasonData {
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContentData {
    content: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelData {
    label: String,
}

pub async fn state(
    ctx: EventContext,
    actor: Actor,
    scope: DashboardScope,
) -> Result<DashboardActionResult, DashboardActionError> {
    if let DashboardScope::Knowledge { query, cursor } = &scope {
        if !tickets_manage(&ctx, &actor)? {
            return Err(DashboardActionError::Forbidden);
        }
        return knowledge::dashboard_state(&ctx, &actor, query.as_deref(), cursor.as_deref()).await;
    }

    let config: Config = ctx.parse_config().map_err(internal)?;
    let can_manage = tickets_manage(&ctx, &actor)?;
    match scope {
        DashboardScope::Guild => {
            let snapshot =
                blacklist::dashboard_state(&ctx, &config, &actor.roles, can_manage, None)
                    .await
                    .map_err(user_error)?;
            let value = if can_manage {
                json!({
                    "entries": snapshot.state.entries,
                    "panels": config.panels.iter().map(panel_summary).collect::<Vec<_>>()
                })
            } else {
                json!({"entries": snapshot.state.entries})
            };
            Ok(result(value, snapshot.revision.unwrap_or(0)))
        }
        DashboardScope::PanelList => {
            let mut panels = Vec::new();
            for panel in &config.panels {
                if panel_visible(&ctx, &config, panel, &actor, can_manage)? {
                    panels.push(panel_summary(panel));
                }
            }
            Ok(result(
                json!({"can_manage_guild": can_manage, "panels": panels}),
                0,
            ))
        }
        DashboardScope::Panel { panel_id } => {
            require_blacklist_access(&config, &actor, can_manage, Some(&panel_id))?;
            let snapshot = blacklist::dashboard_state(
                &ctx,
                &config,
                &actor.roles,
                can_manage,
                Some(&panel_id),
            )
            .await
            .map_err(user_error)?;
            Ok(result(
                json!({"entries": snapshot.state.entries}),
                snapshot.revision.unwrap_or(0),
            ))
        }
        DashboardScope::Ticket { channel_id } => {
            let channel = ChannelId::new(channel_id);
            let entry = ctx
                .store()
                .entry(&ticket_key(channel))
                .await
                .map_err(internal)?
                .ok_or(DashboardActionError::NotFound)?;
            let ticket: Ticket = serde_json::from_value(entry.value).map_err(internal)?;
            if ticket.closed {
                return Err(DashboardActionError::NotFound);
            }
            let access = config.for_ticket(&ticket);
            if !ticket_staff(&ticket, &access, &actor, can_manage) {
                return Err(DashboardActionError::Forbidden);
            }
            Ok(result(
                json!({
                    "status": if ticket.closing.is_some() { "closing" } else { "open" },
                    "waiting_for_reply": ticket.waiting_for_reply,
                    "close_request": ticket.close_request,
                    "labels": ticket.labels
                }),
                entry.revision,
            ))
        }
        DashboardScope::Knowledge { .. } => unreachable!("knowledge scope returned above"),
    }
}

pub async fn action(
    ctx: EventContext,
    actor: Actor,
    request: DashboardActionRequest,
) -> Result<DashboardActionResult, DashboardActionError> {
    if !(1..=64).contains(&request.idempotency_key.len())
        || !request
            .idempotency_key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(DashboardActionError::Invalid(
            "invalid Idempotency-Key".to_owned(),
        ));
    }

    let config: Config = ctx.parse_config().map_err(internal)?;
    let can_manage = tickets_manage(&ctx, &actor)?;
    match (&request.scope, request.action.as_str()) {
        (
            DashboardScope::Knowledge { .. },
            "knowledge.create" | "knowledge.update" | "knowledge.delete",
        ) => {
            if !can_manage {
                return Err(DashboardActionError::Forbidden);
            }
            let start = begin_action(&ctx, &actor, &request).await?;
            let (key, mut receipt, revision) = match start {
                Start::Replay(result) => return Ok(result),
                Start::Claimed {
                    key,
                    receipt,
                    revision,
                    ..
                } => (key, receipt, revision),
            };
            match knowledge::dashboard_action(&ctx, &actor, &request, can_manage).await {
                Ok(result) => complete_action(&ctx, &key, &mut receipt, revision, result).await,
                Err(error) => {
                    fail_action(&ctx, &key, &mut receipt, revision).await;
                    Err(error)
                }
            }
        }
        (DashboardScope::Guild, "blacklist.add" | "blacklist.remove")
        | (DashboardScope::Panel { .. }, "blacklist.add" | "blacklist.remove") => {
            let panel = match &request.scope {
                DashboardScope::Panel { panel_id } => Some(panel_id.as_str()),
                _ => None,
            };
            require_blacklist_access(&config, &actor, can_manage, panel)?;
            blacklist_action(&ctx, &actor, &config, can_manage, &request, panel).await
        }
        (
            DashboardScope::Ticket { channel_id },
            "ticket.close"
            | "ticket.request_close"
            | "ticket.reply"
            | "ticket.add_label"
            | "ticket.remove_label",
        ) => ticket_action(&ctx, &actor, &config, can_manage, &request, *channel_id).await,
        _ => Err(DashboardActionError::Unsupported),
    }
}

async fn blacklist_action(
    ctx: &EventContext,
    actor: &Actor,
    config: &Config,
    can_manage: bool,
    request: &DashboardActionRequest,
    panel: Option<&str>,
) -> Result<DashboardActionResult, DashboardActionError> {
    let data: TargetData = serde_json::from_value(request.data.clone()).map_err(invalid_data)?;
    let target = canonical_target(data.target, ctx.guild_id.get())?;
    let start = begin_action(ctx, actor, request).await?;
    let (key, mut receipt, revision, recovering) = match start {
        Start::Replay(result) => return Ok(result),
        Start::Claimed {
            key,
            receipt,
            revision,
            recovering,
        } => (key, receipt, revision, recovering),
    };

    let snapshot = blacklist::dashboard_state(ctx, config, &actor.roles, can_manage, panel)
        .await
        .map_err(user_error)?;
    let current = snapshot.revision.unwrap_or(0);
    let desired = request.action == "blacklist.add";
    let present = snapshot.state.entries.contains(&target);
    if recovering && present == desired {
        return complete_action(
            ctx,
            &key,
            &mut receipt,
            revision,
            result(
                json!({"entries": snapshot.state.entries, "changed": false}),
                current,
            ),
        )
        .await;
    }
    if current != request.expected_revision {
        fail_action(ctx, &key, &mut receipt, revision).await;
        return Err(DashboardActionError::PreconditionFailed { current });
    }

    let guard = if current == 0 {
        blacklist::RevisionGuard::Missing
    } else {
        blacklist::RevisionGuard::Revision(current)
    };
    match blacklist::dashboard_change(
        ctx,
        config,
        &actor.roles,
        can_manage,
        panel,
        target,
        desired,
        guard,
    )
    .await
    {
        Ok(changed) => {
            complete_action(
                ctx,
                &key,
                &mut receipt,
                revision,
                result(
                    json!({
                        "entries": changed.snapshot.state.entries,
                        "changed": changed.changed
                    }),
                    changed.snapshot.revision.unwrap_or(0),
                ),
            )
            .await
        }
        Err(error) => {
            if let Ok(snapshot) =
                blacklist::dashboard_state(ctx, config, &actor.roles, can_manage, panel).await
            {
                let current = snapshot.revision.unwrap_or(0);
                if current != request.expected_revision {
                    fail_action(ctx, &key, &mut receipt, revision).await;
                    return Err(DashboardActionError::PreconditionFailed { current });
                }
            }
            Err(user_error(error))
        }
    }
}

async fn completed_ticket_action(
    ctx: &EventContext,
    actor: &Actor,
    config: &Config,
    can_manage: bool,
    request: &DashboardActionRequest,
    channel: ChannelId,
) -> Result<Option<DashboardActionResult>, DashboardActionError> {
    let key = receipt_key(ctx, actor, &request.idempotency_key);
    let Some(entry) = ctx.store().entry(&key).await.map_err(internal)? else {
        return Ok(None);
    };
    let receipt: Receipt = serde_json::from_value(entry.value).map_err(internal)?;
    if !same_request(&receipt, actor, request) {
        return Err(DashboardActionError::Conflict);
    }
    let ReceiptState::Complete { value, revision } = receipt.state else {
        return Ok(None);
    };

    if let Some(entry) = ctx
        .store()
        .entry(&ticket_key(channel))
        .await
        .map_err(internal)?
    {
        let ticket: Ticket = serde_json::from_value(entry.value).map_err(internal)?;
        if !ticket.closed {
            let staff = ticket_staff(&ticket, &config.for_ticket(&ticket), actor, can_manage);
            if (request.action == "ticket.close" && may_close(&ticket, actor.user, staff))
                || (request.action != "ticket.close" && staff)
            {
                return Ok(Some(result(value, revision)));
            }
            return Err(DashboardActionError::Forbidden);
        }
    }

    let Some(number) = receipt.ticket_number else {
        return Err(DashboardActionError::NotFound);
    };
    let history = ctx
        .store()
        .get::<super::TicketHistory>(&history_key(number, channel))
        .await
        .map_err(internal)?
        .filter(|history| history.status == HistoryStatus::Closed)
        .ok_or(DashboardActionError::NotFound)?;
    let staff = teams_survey::may_access(
        history.staff_access.as_ref(),
        &history.staff_roles,
        actor.user.get(),
        &actor.roles,
        can_manage,
    );
    if (request.action == "ticket.close" && (staff || history.opener == actor.user.get()))
        || (request.action != "ticket.close" && staff)
    {
        Ok(Some(result(value, revision)))
    } else {
        Err(DashboardActionError::Forbidden)
    }
}

async fn ticket_action(
    ctx: &EventContext,
    actor: &Actor,
    config: &Config,
    can_manage: bool,
    request: &DashboardActionRequest,
    channel_id: u64,
) -> Result<DashboardActionResult, DashboardActionError> {
    let channel = ChannelId::new(channel_id);
    if let Some(result) =
        completed_ticket_action(ctx, actor, config, can_manage, request, channel).await?
    {
        return Ok(result);
    }
    let key = ticket_key(channel);
    let Some(entry) = ctx.store().entry(&key).await.map_err(internal)? else {
        if request.action == "ticket.close"
            && let Some(result) =
                replay_closed_ticket(ctx, actor, request, channel, can_manage).await?
        {
            return Ok(result);
        }
        return Err(DashboardActionError::NotFound);
    };
    let ticket: Ticket = serde_json::from_value(entry.value.clone()).map_err(internal)?;
    if ticket.closed {
        return Err(DashboardActionError::NotFound);
    }
    let access = config.for_ticket(&ticket);
    let staff = ticket_staff(&ticket, &access, actor, can_manage);
    if request.action == "ticket.close" {
        if !may_close(&ticket, actor.user, staff) {
            return Err(DashboardActionError::Forbidden);
        }
    } else if !staff {
        return Err(DashboardActionError::Forbidden);
    }
    if request.action != "ticket.close" && !ticket.is_open() {
        return Err(DashboardActionError::Conflict);
    }

    let start = begin_action(ctx, actor, request).await?;
    let (receipt_key, mut receipt, mut receipt_revision, recovering) = match start {
        Start::Replay(result) => return Ok(result),
        Start::Claimed {
            key,
            receipt,
            revision,
            recovering,
        } => (key, receipt, revision, recovering),
    };
    if receipt.ticket_number.is_none() {
        receipt.ticket_number = Some(ticket.number);
        update_receipt(ctx, &receipt_key, &mut receipt, &mut receipt_revision).await?;
    }
    if !recovering && entry.revision != request.expected_revision {
        fail_action(ctx, &receipt_key, &mut receipt, receipt_revision).await;
        return Err(DashboardActionError::PreconditionFailed {
            current: entry.revision,
        });
    }

    match request.action.as_str() {
        "ticket.close" => {
            let fenced_revision = if ticket.closing.is_some() {
                entry.revision
            } else {
                fence_ticket_revision(
                    ctx,
                    channel,
                    request,
                    &receipt_key,
                    &mut receipt,
                    &mut receipt_revision,
                )
                .await?
            };
            let payload: ReasonData =
                serde_json::from_value(request.data.clone()).map_err(invalid_data)?;
            let reason = normalized_reason(payload.reason, 1000)?;
            let reason = reason.unwrap_or_else(|| {
                ctx.t_in(
                    ctx.locale,
                    "tickets-closed-by",
                    &Args::new().with("user", format!("<@{}>", actor.user.get())),
                )
            });
            match finish(ctx, channel, Some(actor.user), &reason, None).await {
                Ok(super::Closed::Done | super::Closed::RatingDmUnconfirmed) => {
                    complete_action(
                        ctx,
                        &receipt_key,
                        &mut receipt,
                        receipt_revision,
                        result(json!({"closed": true}), fenced_revision),
                    )
                    .await
                }
                Ok(super::Closed::Unknown) => {
                    if let Some(result) =
                        closed_history_result(ctx, &receipt, channel, request.expected_revision)
                            .await?
                    {
                        complete_action(ctx, &receipt_key, &mut receipt, receipt_revision, result)
                            .await
                    } else {
                        fail_action(ctx, &receipt_key, &mut receipt, receipt_revision).await;
                        Err(DashboardActionError::NotFound)
                    }
                }
                Ok(super::Closed::BusyUntil(_)) => Err(DashboardActionError::Conflict),
                Ok(super::Closed::RequestChanged) => {
                    Err(DashboardActionError::PreconditionFailed {
                        current: entry.revision,
                    })
                }
                Err(error) => Err(internal(error)),
            }
        }
        "ticket.request_close" => {
            let payload: ReasonData =
                serde_json::from_value(request.data.clone()).map_err(invalid_data)?;
            let reason = normalized_reason(payload.reason, 1000)?;
            request_close(
                ctx,
                actor,
                &access,
                channel,
                entry.revision,
                reason,
                &receipt_key,
                &mut receipt,
                &mut receipt_revision,
                recovering,
            )
            .await
        }
        "ticket.reply" => {
            let payload: ContentData =
                serde_json::from_value(request.data.clone()).map_err(invalid_data)?;
            let has_attempt = matches!(
                &receipt.state,
                ReceiptState::Pending {
                    step: ReceiptStep::ReplySend { .. },
                    ..
                }
            );
            let reply_revision = if has_attempt {
                entry.revision
            } else {
                fence_ticket_revision(
                    ctx,
                    channel,
                    request,
                    &receipt_key,
                    &mut receipt,
                    &mut receipt_revision,
                )
                .await?
            };
            dashboard_reply(
                ctx,
                actor,
                channel,
                &ticket,
                reply_revision,
                payload.content.trim(),
                &receipt_key,
                &mut receipt,
                &mut receipt_revision,
                recovering,
            )
            .await
        }
        "ticket.add_label" | "ticket.remove_label" => {
            let payload: LabelData =
                serde_json::from_value(request.data.clone()).map_err(invalid_data)?;
            change_label(
                ctx,
                channel,
                &ticket,
                entry.revision,
                &payload.label,
                request.action == "ticket.add_label",
                request,
                &receipt_key,
                &mut receipt,
                receipt_revision,
                recovering,
            )
            .await
        }
        _ => Err(DashboardActionError::Unsupported),
    }
}

async fn fence_ticket_revision(
    ctx: &EventContext,
    channel: ChannelId,
    request: &DashboardActionRequest,
    receipt_key: &str,
    receipt: &mut Receipt,
    receipt_revision: &mut i64,
) -> Result<i64, DashboardActionError> {
    let key = ticket_key(channel);
    let entry = ctx
        .store()
        .entry(&key)
        .await
        .map_err(internal)?
        .ok_or(DashboardActionError::NotFound)?;
    if receipt
        .ticket_revision
        .is_some_and(|revision| revision == entry.revision)
    {
        return Ok(entry.revision);
    }
    if entry.revision != request.expected_revision {
        fail_action(ctx, receipt_key, receipt, *receipt_revision).await;
        return Err(DashboardActionError::PreconditionFailed {
            current: entry.revision,
        });
    }
    let ticket: Ticket = serde_json::from_value(entry.value).map_err(internal)?;
    if !ticket.is_open() {
        return Err(DashboardActionError::Conflict);
    }

    let anticipated = entry
        .revision
        .checked_add(1)
        .ok_or_else(|| DashboardActionError::Internal("ticket revision is exhausted".into()))?;
    receipt.ticket_revision = Some(anticipated);
    update_receipt(ctx, receipt_key, receipt, receipt_revision).await?;
    let next = match ctx
        .store()
        .replace(
            &key,
            &ticket,
            Expect::Revision(entry.revision),
            Some(ticket_ttl(&ticket, ctx.now())),
        )
        .await
    {
        Ok(revision) => revision,
        Err(StorageError::Conflict) => {
            let current = ctx
                .store()
                .entry(&key)
                .await
                .map_err(internal)?
                .map_or(0, |entry| entry.revision);
            fail_action(ctx, receipt_key, receipt, *receipt_revision).await;
            return Err(DashboardActionError::PreconditionFailed { current });
        }
        Err(error) => return Err(internal(error)),
    };
    if receipt.ticket_revision != Some(next) {
        receipt.ticket_revision = Some(next);
        update_receipt(ctx, receipt_key, receipt, receipt_revision).await?;
    }
    Ok(next)
}

async fn request_close(
    ctx: &EventContext,
    actor: &Actor,
    config: &Config,
    channel: ChannelId,
    current_revision: i64,
    reason: Option<String>,
    key: &str,
    receipt: &mut Receipt,
    receipt_revision: &mut i64,
    recovering: bool,
) -> Result<DashboardActionResult, DashboardActionError> {
    let request = if let Some(request) = receipt.close_request.clone() {
        request
    } else {
        let expires_at = match close_request_expiry(ctx.now(), config.close_request_timeout_hours) {
            Some(CloseRequestExpiry::Disabled) => None,
            Some(CloseRequestExpiry::At(deadline)) => Some(deadline),
            None => {
                return Err(DashboardActionError::Invalid(
                    "invalid close request timeout".into(),
                ));
            }
        };
        let id = ctx
            .store()
            .score_add(super::CLOSE_REQUESTS, 0_u64, 1)
            .await
            .map_err(internal)?;
        if id <= 0 {
            return Err(DashboardActionError::Unavailable);
        }
        let request = CloseRequest {
            id,
            requested_by: actor.user.get(),
            reason,
            expires_at,
        };
        receipt.close_request = Some(request.clone());
        update_receipt(ctx, key, receipt, receipt_revision).await?;
        request
    };

    let entry = ctx
        .store()
        .entry(&ticket_key(channel))
        .await
        .map_err(internal)?
        .ok_or(DashboardActionError::NotFound)?;
    let mut latest: Ticket = serde_json::from_value(entry.value).map_err(internal)?;
    if latest.closed || latest.closing.is_some() {
        return Err(DashboardActionError::Conflict);
    }
    if let Some(existing) = latest.close_request.as_ref() {
        if existing.id != request.id {
            return Err(DashboardActionError::Conflict);
        }
    } else if entry.revision != receipt.expected_revision {
        return Err(DashboardActionError::PreconditionFailed {
            current: entry.revision,
        });
    } else if recovering && current_revision != receipt.expected_revision {
        return Err(DashboardActionError::PreconditionFailed {
            current: current_revision,
        });
    }

    if let Some(deadline) = request.expires_at {
        watch_close_request_timeout(ctx, channel, request.id, deadline, deadline)
            .await
            .map_err(internal)?;
    }
    if latest.close_request.is_none() {
        latest.close_request = Some(request.clone());
        refresh_ticket_form_answer_ttl(ctx, &latest)
            .await
            .map_err(internal)?;
        match ctx
            .store()
            .replace(
                &ticket_key(channel),
                &latest,
                Expect::Revision(entry.revision),
                Some(ticket_ttl(&latest, ctx.now())),
            )
            .await
        {
            Ok(_) => {}
            Err(StorageError::Conflict) => {
                cancel_close_request_timeout(ctx, channel, &request).await;
                let current = ctx
                    .store()
                    .entry(&ticket_key(channel))
                    .await
                    .map_err(internal)?
                    .map_or(0, |entry| entry.revision);
                return Err(DashboardActionError::PreconditionFailed { current });
            }
            Err(error) => {
                cancel_close_request_timeout(ctx, channel, &request).await;
                return Err(internal(error));
            }
        }
    }

    set_step(
        ctx,
        key,
        receipt,
        receipt_revision,
        ReceiptStep::RequestMessage,
    )
    .await?;
    if let Err(error) = post_close_request_message(ctx, channel, latest.opener, &request).await {
        mark_uncertain(ctx, key, receipt, receipt_revision).await;
        return Err(internal(error));
    }
    let revision = ctx
        .store()
        .entry(&ticket_key(channel))
        .await
        .map_err(internal)?
        .map_or(current_revision, |entry| entry.revision);
    complete_action(
        ctx,
        key,
        receipt,
        *receipt_revision,
        result(json!({"close_request": {"id": request.id}}), revision),
    )
    .await
}

async fn dashboard_reply(
    ctx: &EventContext,
    actor: &Actor,
    channel: ChannelId,
    ticket: &Ticket,
    current_revision: i64,
    content: &str,
    receipt_key: &str,
    receipt: &mut Receipt,
    receipt_revision: &mut i64,
    recovering: bool,
) -> Result<DashboardActionResult, DashboardActionError> {
    if !valid_reply_content("dashboard", content) {
        return Err(DashboardActionError::Invalid("invalid ticket reply".into()));
    }
    let attempt_id = match &receipt.state {
        ReceiptState::Pending {
            step: ReceiptStep::ReplySend { attempt_id },
            ..
        } => *attempt_id,
        _ => {
            let attempt_id = ctx
                .store()
                .counter_add(super::REPLY_SEND_COUNTER, actor.user.get(), 1)
                .await
                .map_err(internal)?;
            let attempt = ReplySendAttempt {
                channel: channel.get(),
                owner: actor.user.get(),
                ticket_number: ticket.number,
                panel: ticket.panel.clone(),
                reply_id: "dashboard".to_owned(),
                chunks: split_reply(content)
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                confirmed: 0,
                in_flight: None,
            };
            let attempt_key = reply_send_attempt_key(channel, actor.user.get(), attempt_id);
            ctx.store()
                .replace(
                    &attempt_key,
                    &attempt,
                    Expect::Missing,
                    Some(super::REPLY_SEND_RETENTION),
                )
                .await
                .map_err(internal)?;
            set_step(
                ctx,
                receipt_key,
                receipt,
                receipt_revision,
                ReceiptStep::ReplySend { attempt_id },
            )
            .await?;
            attempt_id
        }
    };
    let attempt_key = reply_send_attempt_key(channel, actor.user.get(), attempt_id);
    let entry = ctx
        .store()
        .entry(&attempt_key)
        .await
        .map_err(internal)?
        .ok_or(DashboardActionError::Unavailable)?;
    let mut attempt: ReplySendAttempt = serde_json::from_value(entry.value).map_err(internal)?;
    if !super::valid_reply_send_attempt(&attempt, ticket, channel, actor.user.get()) {
        return Err(DashboardActionError::Internal(
            "invalid saved dashboard reply attempt".into(),
        ));
    }
    if attempt.confirmed == attempt.chunks.len() && attempt.in_flight.is_none() {
        return complete_action(
            ctx,
            receipt_key,
            receipt,
            *receipt_revision,
            result(
                json!({"sent": true, "parts": attempt.chunks.len()}),
                current_revision,
            ),
        )
        .await;
    }
    if attempt.in_flight.is_some() {
        mark_uncertain(ctx, receipt_key, receipt, receipt_revision).await;
        return Err(DashboardActionError::Unavailable);
    }
    if recovering
        && receipt
            .ticket_revision
            .is_some_and(|revision| revision != current_revision)
    {
        mark_uncertain(ctx, receipt_key, receipt, receipt_revision).await;
        return Err(DashboardActionError::PreconditionFailed {
            current: current_revision,
        });
    }

    let mut revision = entry.revision;
    for index in attempt.confirmed..attempt.chunks.len() {
        attempt.in_flight = Some(index);
        revision = ctx
            .store()
            .replace(
                &attempt_key,
                &attempt,
                Expect::Revision(revision),
                Some(super::REPLY_SEND_RETENTION),
            )
            .await
            .map_err(internal)?;
        if let Err(error) = ctx
            .api
            .execute(Action::SendMessage {
                channel_id: channel,
                content: Some(attempt.chunks[index].clone()),
                embeds: Vec::new(),
                reply_to: None,
                mentions: MentionPolicy::None,
            })
            .await
        {
            mark_uncertain(ctx, receipt_key, receipt, receipt_revision).await;
            return Err(internal(error));
        }
        attempt.confirmed = index + 1;
        attempt.in_flight = None;
        match ctx
            .store()
            .replace(
                &attempt_key,
                &attempt,
                Expect::Revision(revision),
                Some(super::REPLY_SEND_RETENTION),
            )
            .await
        {
            Ok(next) => revision = next,
            Err(error) => {
                mark_uncertain(ctx, receipt_key, receipt, receipt_revision).await;
                return Err(internal(error));
            }
        }
        renew_receipt(ctx, receipt_key, receipt, receipt_revision).await?;
    }
    complete_action(
        ctx,
        receipt_key,
        receipt,
        *receipt_revision,
        result(
            json!({"sent": true, "parts": attempt.chunks.len()}),
            current_revision,
        ),
    )
    .await
}

async fn change_label(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
    current_revision: i64,
    raw_label: &str,
    add: bool,
    request: &DashboardActionRequest,
    key: &str,
    receipt: &mut Receipt,
    receipt_revision: i64,
    recovering: bool,
) -> Result<DashboardActionResult, DashboardActionError> {
    let label = super::display_label(raw_label)
        .ok_or_else(|| DashboardActionError::Invalid("invalid ticket label".to_owned()))?;
    let normalized = label_key(&label);
    let present = ticket
        .labels
        .iter()
        .any(|value| label_key(value) == normalized);
    if recovering && current_revision != request.expected_revision {
        if present == add {
            return complete_action(
                ctx,
                key,
                receipt,
                receipt_revision,
                result(
                    json!({"labels": ticket.labels, "changed": false}),
                    current_revision,
                ),
            )
            .await;
        }
        return Err(DashboardActionError::PreconditionFailed {
            current: current_revision,
        });
    }
    if current_revision != request.expected_revision {
        return Err(DashboardActionError::PreconditionFailed {
            current: current_revision,
        });
    }
    if present == add {
        return complete_action(
            ctx,
            key,
            receipt,
            receipt_revision,
            result(
                json!({"labels": ticket.labels, "changed": false}),
                current_revision,
            ),
        )
        .await;
    }

    let mut updated = ticket.clone();
    if add {
        updated.labels.push(label);
        updated.labels.sort_by_cached_key(|value| label_key(value));
    } else {
        updated
            .labels
            .retain(|value| label_key(value) != normalized);
    }
    refresh_ticket_form_answer_ttl(ctx, &updated)
        .await
        .map_err(internal)?;
    let next = match ctx
        .store()
        .replace(
            &ticket_key(channel),
            &updated,
            Expect::Revision(current_revision),
            Some(ticket_ttl(&updated, ctx.now())),
        )
        .await
    {
        Ok(revision) => revision,
        Err(StorageError::Conflict) => {
            let current = ctx
                .store()
                .entry(&ticket_key(channel))
                .await
                .map_err(internal)?
                .map_or(0, |entry| entry.revision);
            fail_action(ctx, key, receipt, receipt_revision).await;
            return Err(DashboardActionError::PreconditionFailed { current });
        }
        Err(error) => return Err(internal(error)),
    };
    complete_action(
        ctx,
        key,
        receipt,
        receipt_revision,
        result(json!({"labels": updated.labels, "changed": true}), next),
    )
    .await
}

async fn replay_closed_ticket(
    ctx: &EventContext,
    actor: &Actor,
    request: &DashboardActionRequest,
    channel: ChannelId,
    can_manage: bool,
) -> Result<Option<DashboardActionResult>, DashboardActionError> {
    let key = receipt_key(ctx, actor, &request.idempotency_key);
    let Some(entry) = ctx.store().entry(&key).await.map_err(internal)? else {
        return Ok(None);
    };
    let mut receipt: Receipt = serde_json::from_value(entry.value).map_err(internal)?;
    if !same_request(&receipt, actor, request) {
        return Err(DashboardActionError::Conflict);
    }
    if let ReceiptState::Complete { value, revision } = receipt.state {
        return Ok(Some(DashboardActionResult { value, revision }));
    }
    if !matches!(
        &receipt.state,
        ReceiptState::Pending {
            lease_until,
            step: ReceiptStep::Ready | ReceiptStep::ReplySend { .. }
        } if *lease_until <= ctx.now()
    ) {
        return Ok(None);
    }
    let Some(number) = receipt.ticket_number else {
        return Ok(None);
    };
    let Some(history) = ctx
        .store()
        .get::<super::TicketHistory>(&history_key(number, channel))
        .await
        .map_err(internal)?
        .filter(|history| history.status == HistoryStatus::Closed)
    else {
        return Ok(None);
    };
    let staff = teams_survey::may_access(
        history.staff_access.as_ref(),
        &history.staff_roles,
        actor.user.get(),
        &actor.roles,
        can_manage,
    );
    if !staff && history.opener != actor.user.get() {
        return Err(DashboardActionError::Forbidden);
    }
    let result = result(json!({"closed": true}), request.expected_revision);
    let revision = entry.revision;
    complete_action(ctx, &key, &mut receipt, revision, result.clone()).await?;
    Ok(Some(result))
}

async fn closed_history_result(
    ctx: &EventContext,
    receipt: &Receipt,
    channel: ChannelId,
    revision: i64,
) -> Result<Option<DashboardActionResult>, DashboardActionError> {
    let Some(number) = receipt.ticket_number else {
        return Ok(None);
    };
    let history = ctx
        .store()
        .get::<super::TicketHistory>(&history_key(number, channel))
        .await
        .map_err(internal)?;
    Ok(history
        .filter(|history| history.status == HistoryStatus::Closed)
        .map(|_| result(json!({"closed": true}), revision)))
}

async fn begin_action(
    ctx: &EventContext,
    actor: &Actor,
    request: &DashboardActionRequest,
) -> Result<Start, DashboardActionError> {
    let key = receipt_key(ctx, actor, &request.idempotency_key);
    if let Some(entry) = ctx.store().entry(&key).await.map_err(internal)? {
        let mut receipt: Receipt = serde_json::from_value(entry.value).map_err(internal)?;
        if !same_request(&receipt, actor, request) {
            return Err(DashboardActionError::Conflict);
        }
        match receipt.state.clone() {
            ReceiptState::Complete { value, revision } => {
                return Ok(Start::Replay(DashboardActionResult { value, revision }));
            }
            ReceiptState::Uncertain => return Err(DashboardActionError::Unavailable),
            ReceiptState::Failed => return Err(DashboardActionError::Conflict),
            ReceiptState::Pending { lease_until, step } => {
                if lease_until > ctx.now() {
                    return Err(DashboardActionError::Conflict);
                }
                if matches!(&step, ReceiptStep::RequestMessage) {
                    let mut revision = entry.revision;
                    mark_uncertain(ctx, &key, &mut receipt, &mut revision).await;
                    return Err(DashboardActionError::Unavailable);
                }
                receipt.state = ReceiptState::Pending {
                    lease_until: ctx.now() + RECEIPT_LEASE_SECONDS,
                    step,
                };
                let mut revision = entry.revision;
                update_receipt(ctx, &key, &mut receipt, &mut revision).await?;
                return Ok(Start::Claimed {
                    key,
                    receipt,
                    revision,
                    recovering: true,
                });
            }
        }
    }

    let receipt = Receipt {
        actor: actor.user.get(),
        scope: scope_identity(&request.scope),
        action: request.action.clone(),
        data: request.data.clone(),
        expected_revision: request.expected_revision,
        ticket_revision: None,
        ticket_number: None,
        close_request: None,
        state: ReceiptState::Pending {
            lease_until: ctx.now() + RECEIPT_LEASE_SECONDS,
            step: ReceiptStep::Ready,
        },
    };
    let revision = ctx
        .store()
        .replace(&key, &receipt, Expect::Missing, Some(RECEIPT_RETENTION))
        .await
        .map_err(|error| match error {
            StorageError::Conflict => DashboardActionError::Conflict,
            other => internal(other),
        })?;
    Ok(Start::Claimed {
        key,
        receipt,
        revision,
        recovering: false,
    })
}

async fn update_receipt(
    ctx: &EventContext,
    key: &str,
    receipt: &mut Receipt,
    revision: &mut i64,
) -> Result<(), DashboardActionError> {
    *revision = ctx
        .store()
        .replace(
            key,
            receipt,
            Expect::Revision(*revision),
            Some(RECEIPT_RETENTION),
        )
        .await
        .map_err(|error| match error {
            StorageError::Conflict => DashboardActionError::Conflict,
            other => internal(other),
        })?;
    Ok(())
}

async fn renew_receipt(
    ctx: &EventContext,
    key: &str,
    receipt: &mut Receipt,
    revision: &mut i64,
) -> Result<(), DashboardActionError> {
    if let ReceiptState::Pending { step, .. } = receipt.state.clone() {
        receipt.state = ReceiptState::Pending {
            lease_until: ctx.now() + RECEIPT_LEASE_SECONDS,
            step,
        };
    }
    update_receipt(ctx, key, receipt, revision).await
}

async fn set_step(
    ctx: &EventContext,
    key: &str,
    receipt: &mut Receipt,
    revision: &mut i64,
    step: ReceiptStep,
) -> Result<(), DashboardActionError> {
    receipt.state = ReceiptState::Pending {
        lease_until: ctx.now() + RECEIPT_LEASE_SECONDS,
        step,
    };
    update_receipt(ctx, key, receipt, revision).await
}

async fn complete_action(
    ctx: &EventContext,
    key: &str,
    receipt: &mut Receipt,
    revision: i64,
    result: DashboardActionResult,
) -> Result<DashboardActionResult, DashboardActionError> {
    receipt.state = ReceiptState::Complete {
        value: result.value.clone(),
        revision: result.revision,
    };
    let mut revision = revision;
    update_receipt(ctx, key, receipt, &mut revision)
        .await
        .map_err(|_| DashboardActionError::Unavailable)?;
    Ok(result)
}

async fn fail_action(ctx: &EventContext, key: &str, receipt: &mut Receipt, revision: i64) {
    receipt.state = ReceiptState::Failed;
    let mut revision = revision;
    let _ = update_receipt(ctx, key, receipt, &mut revision).await;
}

async fn mark_uncertain(ctx: &EventContext, key: &str, receipt: &mut Receipt, revision: &mut i64) {
    receipt.state = ReceiptState::Uncertain;
    let _ = update_receipt(ctx, key, receipt, revision).await;
}

fn same_request(receipt: &Receipt, actor: &Actor, request: &DashboardActionRequest) -> bool {
    receipt.actor == actor.user.get()
        && receipt.scope == scope_identity(&request.scope)
        && receipt.action == request.action
        && receipt.data == request.data
        && receipt.expected_revision == request.expected_revision
}

fn receipt_key(ctx: &EventContext, actor: &Actor, idempotency_key: &str) -> String {
    format!(
        "{RECEIPT_PREFIX}{}:{}:{idempotency_key}",
        ctx.guild_id.get(),
        actor.user.get()
    )
}

fn scope_identity(scope: &DashboardScope) -> String {
    match scope {
        DashboardScope::Guild => "guild".into(),
        DashboardScope::PanelList => "panels".into(),
        DashboardScope::Panel { panel_id } => format!("panel:{panel_id}"),
        DashboardScope::Ticket { channel_id } => format!("ticket:{channel_id}"),
        DashboardScope::Knowledge { query, cursor } => {
            format!("knowledge:{query:?}:{cursor:?}")
        }
    }
}

fn tickets_manage(ctx: &EventContext, actor: &Actor) -> Result<bool, DashboardActionError> {
    let key = PermissionKey::new("tickets.manage").map_err(internal)?;
    Ok(ctx.permissions().check(actor, &key).allowed)
}

fn ticket_staff(ticket: &Ticket, config: &Config, actor: &Actor, can_manage: bool) -> bool {
    teams_survey::may_access(
        ticket.staff_access.as_ref(),
        &history_staff_roles(ticket, config),
        actor.user.get(),
        &actor.roles,
        can_manage,
    )
}

fn panel_visible(
    ctx: &EventContext,
    config: &Config,
    panel: &super::NamedPanel,
    actor: &Actor,
    can_manage: bool,
) -> Result<bool, DashboardActionError> {
    let scoped = config.with_panel(panel);
    let roles = scoped.staff();
    let legacy_roles = roles.iter().map(|role| role.get()).collect::<Vec<_>>();
    let snapshot = teams_survey::staff_access_snapshot(
        &config.staff_teams,
        &panel.staff_teams,
        &roles,
        ctx.guild_id.get(),
    )
    .map_err(user_error)?;
    Ok(teams_survey::may_access(
        snapshot.as_ref(),
        &legacy_roles,
        actor.user.get(),
        &actor.roles,
        can_manage,
    ))
}

fn require_blacklist_access(
    config: &Config,
    actor: &Actor,
    can_manage: bool,
    panel: Option<&str>,
) -> Result<(), DashboardActionError> {
    let scope =
        blacklist::scope_config(config, panel).map_err(|_| DashboardActionError::NotFound)?;
    if blacklist::staff_for_actor(&scope, &actor.roles, can_manage) {
        Ok(())
    } else {
        Err(DashboardActionError::Forbidden)
    }
}

fn canonical_target(
    target: blacklist::Target,
    guild_id: u64,
) -> Result<blacklist::Target, DashboardActionError> {
    let (role, raw_id) = match target {
        blacklist::Target::User { id } => (false, id),
        blacklist::Target::Role { id } => (true, id),
    };
    let id = raw_id
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0 && (!role || *id != guild_id))
        .ok_or_else(|| DashboardActionError::Invalid("invalid blacklist target".into()))?
        .to_string();
    Ok(if role {
        blacklist::Target::Role { id }
    } else {
        blacklist::Target::User { id }
    })
}

fn panel_summary(panel: &super::NamedPanel) -> Value {
    json!({
        "id": panel.id,
        "name": panel.name.as_deref().unwrap_or(&panel.id)
    })
}

fn normalized_reason(
    reason: Option<String>,
    max_chars: usize,
) -> Result<Option<String>, DashboardActionError> {
    let Some(reason) = reason
        .map(|reason| reason.trim().to_owned())
        .filter(|reason| !reason.is_empty())
    else {
        return Ok(None);
    };
    if reason.chars().count() > max_chars || reason.contains('\0') {
        return Err(DashboardActionError::Invalid("invalid reason".into()));
    }
    Ok(Some(reason))
}

fn result(value: Value, revision: i64) -> DashboardActionResult {
    DashboardActionResult { value, revision }
}

fn invalid_data(error: impl Display) -> DashboardActionError {
    DashboardActionError::Invalid(error.to_string())
}

fn user_error(error: super::UserError) -> DashboardActionError {
    DashboardActionError::Internal(format!("{error:?}"))
}

fn internal(error: impl Display) -> DashboardActionError {
    DashboardActionError::Internal(error.to_string())
}
