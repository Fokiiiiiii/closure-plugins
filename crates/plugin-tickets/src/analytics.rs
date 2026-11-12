//! Bounded analytics over accessible retained ticket histories and current tickets.

use std::fmt::Display;
use std::time::Duration;

use closure_core::module::{DashboardActionError, DashboardActionResult, EventContext};
use closure_core::permission::{Actor, PermissionKey};
use closure_core::storage::{Expect, StorageError};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Config, HistoryStatus, Ticket, TicketHistory, history_staff_roles, teams_survey};

const HISTORY_PREFIX: &str = "ticket-history:";
const TICKET_PREFIX: &str = "ticket:";
const JOB_PREFIX: &str = "ticket-dashboard-analytics:";
const JOB_COUNTER: &str = "ticket-dashboard-analytics-job";
const BATCH_SIZE: usize = 250;
const JOB_TTL: Duration = Duration::from_secs(15 * 60);
const SECONDS_PER_DAY: i64 = 86_400;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AccessSnapshot {
    user: u64,
    roles: Vec<u64>,
    administrator: bool,
    owner: bool,
    can_manage: bool,
    legacy_staff_roles: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    Histories,
    OpenTickets,
    Complete,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Totals {
    closed_count: u64,
    duration_samples: u64,
    duration_missing: u64,
    duration_total_seconds: u64,
    rating_count: u64,
    rating_missing: u64,
    rating_score_sum: u64,
    rating_distribution: [u64; 5],
    open_count: u64,
    records_scanned: u64,
    has_unreadable_records: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AnalyticsJob {
    guild: u64,
    actor: u64,
    period_days: u16,
    period_start: i64,
    period_end: i64,
    access: AccessSnapshot,
    phase: Phase,
    after_key: Option<String>,
    totals: Totals,
}

pub(super) async fn dashboard_state(
    ctx: &EventContext,
    actor: &Actor,
    period_days: u16,
    cursor: Option<&str>,
) -> Result<DashboardActionResult, DashboardActionError> {
    if !matches!(period_days, 7 | 30 | 90) {
        return Err(DashboardActionError::Invalid(
            "unsupported analytics period".to_owned(),
        ));
    }
    let config: Config = ctx.parse_config().map_err(internal)?;
    let access = access_snapshot(ctx, actor, &config)?;
    match cursor {
        Some(cursor) => continue_job(ctx, actor, period_days, cursor, &access).await,
        None => start_job(ctx, actor, period_days, &config, access).await,
    }
}

async fn start_job(
    ctx: &EventContext,
    actor: &Actor,
    period_days: u16,
    config: &Config,
    access: AccessSnapshot,
) -> Result<DashboardActionResult, DashboardActionError> {
    let period_end = ctx.now();
    let seconds = i64::from(period_days)
        .checked_mul(SECONDS_PER_DAY)
        .ok_or_else(|| DashboardActionError::Invalid("invalid analytics period".into()))?;
    let period_start = period_end
        .checked_sub(seconds)
        .ok_or_else(|| DashboardActionError::Invalid("invalid analytics range".into()))?;
    let id = ctx
        .store()
        .counter_add(JOB_COUNTER, actor.user.get(), 1)
        .await
        .map_err(internal)?;
    if id <= 0 {
        return Err(DashboardActionError::Unavailable);
    }
    let cursor = id.to_string();
    let key = job_key(ctx, actor, period_days, &cursor);
    let mut job = AnalyticsJob {
        guild: ctx.guild_id.get(),
        actor: actor.user.get(),
        period_days,
        period_start,
        period_end,
        access,
        phase: Phase::Histories,
        after_key: None,
        totals: Totals::default(),
    };
    let revision = ctx
        .store()
        .replace(&key, &job, Expect::Missing, Some(JOB_TTL))
        .await
        .map_err(|error| match error {
            StorageError::Conflict => DashboardActionError::Conflict,
            other => internal(other),
        })?;
    advance(ctx, actor, config, &mut job).await?;
    let revision = persist_job(ctx, &key, &job, revision).await?;
    Ok(job_result(&job, &cursor, revision))
}

async fn continue_job(
    ctx: &EventContext,
    actor: &Actor,
    period_days: u16,
    cursor: &str,
    access: &AccessSnapshot,
) -> Result<DashboardActionResult, DashboardActionError> {
    if cursor.is_empty() || cursor.len() > 20 || !cursor.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DashboardActionError::Invalid(
            "invalid analytics cursor".into(),
        ));
    }
    let key = job_key(ctx, actor, period_days, cursor);
    let entry = ctx
        .store()
        .entry(&key)
        .await
        .map_err(internal)?
        .ok_or(DashboardActionError::NotFound)?;
    let mut job: AnalyticsJob = serde_json::from_value(entry.value).map_err(internal)?;
    if job.guild != ctx.guild_id.get()
        || job.actor != actor.user.get()
        || job.period_days != period_days
        || &job.access != access
    {
        return Err(DashboardActionError::Conflict);
    }
    if job.phase == Phase::Complete {
        return Ok(job_result(&job, cursor, entry.revision));
    }

    let config: Config = ctx.parse_config().map_err(internal)?;
    advance(ctx, actor, &config, &mut job).await?;
    match ctx
        .store()
        .replace(&key, &job, Expect::Revision(entry.revision), Some(JOB_TTL))
        .await
    {
        Ok(revision) => Ok(job_result(&job, cursor, revision)),
        Err(StorageError::Conflict) => {
            let latest = ctx
                .store()
                .entry(&key)
                .await
                .map_err(internal)?
                .ok_or(DashboardActionError::NotFound)?;
            let job: AnalyticsJob = serde_json::from_value(latest.value).map_err(internal)?;
            if job.guild != ctx.guild_id.get()
                || job.actor != actor.user.get()
                || job.period_days != period_days
                || &job.access != access
            {
                return Err(DashboardActionError::Conflict);
            }
            Ok(job_result(&job, cursor, latest.revision))
        }
        Err(error) => Err(internal(error)),
    }
}

async fn persist_job(
    ctx: &EventContext,
    key: &str,
    job: &AnalyticsJob,
    revision: i64,
) -> Result<i64, DashboardActionError> {
    ctx.store()
        .replace(key, job, Expect::Revision(revision), Some(JOB_TTL))
        .await
        .map_err(|error| match error {
            StorageError::Conflict => DashboardActionError::Conflict,
            other => internal(other),
        })
}

async fn advance(
    ctx: &EventContext,
    actor: &Actor,
    config: &Config,
    job: &mut AnalyticsJob,
) -> Result<(), DashboardActionError> {
    match job.phase {
        Phase::Histories => scan_histories(ctx, actor, config, job).await,
        Phase::OpenTickets => scan_open_tickets(ctx, actor, config, job).await,
        Phase::Complete => Ok(()),
    }
}

async fn scan_histories(
    ctx: &EventContext,
    actor: &Actor,
    config: &Config,
    job: &mut AnalyticsJob,
) -> Result<(), DashboardActionError> {
    let entries = match job.after_key.as_deref() {
        Some(after) => {
            ctx.store()
                .list_after(HISTORY_PREFIX, after, BATCH_SIZE)
                .await
        }
        None => ctx.store().list(HISTORY_PREFIX, BATCH_SIZE).await,
    }
    .map_err(internal)?;
    if entries.is_empty() {
        job.phase = Phase::OpenTickets;
        job.after_key = None;
        return Ok(());
    }
    let exhausted = entries.len() < BATCH_SIZE;
    for (key, entry) in entries {
        job.after_key = Some(key);
        let history: TicketHistory = match serde_json::from_value(entry.value) {
            Ok(history) => history,
            Err(_) => {
                job.totals.has_unreadable_records = true;
                continue;
            }
        };
        if !history_visible(&history, actor, config, job.access.can_manage) {
            continue;
        }
        job.totals.records_scanned = job.totals.records_scanned.saturating_add(1);
        accumulate_history(&mut job.totals, &history, job.period_start, job.period_end);
    }
    if exhausted {
        job.phase = Phase::OpenTickets;
        job.after_key = None;
    }
    Ok(())
}

async fn scan_open_tickets(
    ctx: &EventContext,
    actor: &Actor,
    config: &Config,
    job: &mut AnalyticsJob,
) -> Result<(), DashboardActionError> {
    let entries = match job.after_key.as_deref() {
        Some(after) => {
            ctx.store()
                .list_after(TICKET_PREFIX, after, BATCH_SIZE)
                .await
        }
        None => ctx.store().list(TICKET_PREFIX, BATCH_SIZE).await,
    }
    .map_err(internal)?;
    if entries.is_empty() {
        job.phase = Phase::Complete;
        job.after_key = None;
        return Ok(());
    }
    let exhausted = entries.len() < BATCH_SIZE;
    for (key, entry) in entries {
        job.after_key = Some(key.clone());
        if !key
            .strip_prefix(TICKET_PREFIX)
            .and_then(|id| id.parse::<u64>().ok())
            .is_some_and(|id| id > 0)
        {
            job.totals.has_unreadable_records = true;
            continue;
        }
        let ticket: Ticket = match serde_json::from_value(entry.value) {
            Ok(ticket) => ticket,
            Err(_) => {
                job.totals.has_unreadable_records = true;
                continue;
            }
        };
        let access = config.for_ticket(&ticket);
        let roles = history_staff_roles(&ticket, &access);
        if !teams_survey::may_access(
            ticket.staff_access.as_ref(),
            &roles,
            actor.user.get(),
            &actor.roles,
            job.access.can_manage,
        ) {
            continue;
        }
        job.totals.records_scanned = job.totals.records_scanned.saturating_add(1);
        if ticket.is_open() {
            if ticket.created <= 0 {
                job.totals.has_unreadable_records = true;
            } else if in_period(ticket.created, job.period_start, job.period_end) {
                job.totals.open_count = job.totals.open_count.saturating_add(1);
            }
        }
    }
    if exhausted {
        job.phase = Phase::Complete;
        job.after_key = None;
    }
    Ok(())
}

fn accumulate_history(
    totals: &mut Totals,
    history: &TicketHistory,
    period_start: i64,
    period_end: i64,
) {
    if history.closed <= 0 {
        totals.has_unreadable_records = true;
        return;
    }
    let closed_in_period = in_period(history.closed, period_start, period_end);
    let rating_in_period = history
        .rating
        .as_ref()
        .is_some_and(|rating| in_period(rating.created, period_start, period_end));
    if history.status != HistoryStatus::Closed {
        if closed_in_period || rating_in_period {
            totals.has_unreadable_records = true;
        }
        return;
    }
    if history
        .rating
        .as_ref()
        .is_some_and(|rating| rating.created <= 0 || !(1..=5).contains(&rating.score))
        && closed_in_period
    {
        totals.has_unreadable_records = true;
    }

    if closed_in_period {
        totals.closed_count = totals.closed_count.saturating_add(1);
        if history.created > 0 && history.closed >= history.created {
            totals.duration_samples = totals.duration_samples.saturating_add(1);
            totals.duration_total_seconds = totals
                .duration_total_seconds
                .saturating_add((history.closed - history.created) as u64);
        } else {
            totals.duration_missing = totals.duration_missing.saturating_add(1);
        }
    }

    let valid_rating = history
        .rating
        .as_ref()
        .filter(|rating| rating.created > 0 && (1..=5).contains(&rating.score));
    if rating_in_period {
        if let Some(rating) = valid_rating {
            totals.rating_count = totals.rating_count.saturating_add(1);
            totals.rating_score_sum = totals
                .rating_score_sum
                .saturating_add(u64::from(rating.score));
            let index = usize::from(rating.score - 1);
            totals.rating_distribution[index] = totals.rating_distribution[index].saturating_add(1);
        } else {
            totals.has_unreadable_records = true;
        }
    }
    if closed_in_period
        && !history.rating.as_ref().is_some_and(|rating| {
            rating_in_period && rating.created > 0 && (1..=5).contains(&rating.score)
        })
    {
        totals.rating_missing = totals.rating_missing.saturating_add(1);
    }
}

fn history_visible(
    history: &TicketHistory,
    actor: &Actor,
    config: &Config,
    can_manage: bool,
) -> bool {
    let roles = if history.staff_roles.is_empty() {
        config.staff().iter().map(|role| role.get()).collect()
    } else {
        history.staff_roles.clone()
    };
    teams_survey::may_access(
        history.staff_access.as_ref(),
        &roles,
        actor.user.get(),
        &actor.roles,
        can_manage,
    )
}

fn access_snapshot(
    ctx: &EventContext,
    actor: &Actor,
    config: &Config,
) -> Result<AccessSnapshot, DashboardActionError> {
    let key = PermissionKey::new("tickets.manage").map_err(internal)?;
    let mut roles = actor
        .roles
        .iter()
        .map(|role| role.get())
        .collect::<Vec<_>>();
    roles.sort_unstable();
    roles.dedup();
    let mut legacy_staff_roles = config.staff_roles.clone();
    legacy_staff_roles.sort();
    legacy_staff_roles.dedup();
    Ok(AccessSnapshot {
        user: actor.user.get(),
        roles,
        administrator: actor.administrator,
        owner: actor.owner,
        can_manage: ctx.permissions().check(actor, &key).allowed,
        legacy_staff_roles,
    })
}

fn job_key(ctx: &EventContext, actor: &Actor, period_days: u16, cursor: &str) -> String {
    format!(
        "{JOB_PREFIX}{}:{}:{period_days}:{cursor}",
        ctx.guild_id.get(),
        actor.user.get()
    )
}

fn job_result(job: &AnalyticsJob, cursor: &str, revision: i64) -> DashboardActionResult {
    let complete = job.phase == Phase::Complete;
    let duration_average = (job.totals.duration_samples > 0)
        .then(|| job.totals.duration_total_seconds as f64 / job.totals.duration_samples as f64);
    let rating_average = (job.totals.rating_count > 0)
        .then(|| job.totals.rating_score_sum as f64 / job.totals.rating_count as f64);
    DashboardActionResult {
        value: json!({
            "period_days": job.period_days,
            "period_start": job.period_start,
            "period_end": job.period_end,
            "closed_count": job.totals.closed_count,
            "duration": {
                "samples": job.totals.duration_samples,
                "missing": job.totals.duration_missing,
                "total_seconds": job.totals.duration_total_seconds,
                "average_seconds": duration_average
            },
            "ratings": {
                "count": job.totals.rating_count,
                "missing": job.totals.rating_missing,
                "score_sum": job.totals.rating_score_sum,
                "average": rating_average,
                "distribution": {
                    "1": job.totals.rating_distribution[0],
                    "2": job.totals.rating_distribution[1],
                    "3": job.totals.rating_distribution[2],
                    "4": job.totals.rating_distribution[3],
                    "5": job.totals.rating_distribution[4]
                }
            },
            "open_count": job.totals.open_count,
            "records_scanned": job.totals.records_scanned,
            "complete": complete,
            "data_complete": complete && !job.totals.has_unreadable_records,
            "has_unreadable_records": job.totals.has_unreadable_records,
            "history_retention_days": super::HISTORY_TTL.as_secs() / SECONDS_PER_DAY as u64,
            "source": "retained TicketHistory and current open tickets",
            "next_cursor": (!complete).then_some(cursor)
        }),
        revision,
    }
}

fn in_period(timestamp: i64, start: i64, end: i64) -> bool {
    timestamp >= start && timestamp < end
}

fn internal(error: impl Display) -> DashboardActionError {
    DashboardActionError::Internal(error.to_string())
}
