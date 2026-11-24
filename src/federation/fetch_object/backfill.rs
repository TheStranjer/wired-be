//! The `federator.backfill` job: walk a thread up from a reply whose parent
//! this instance does not have, one link per run.

use std::fmt;

use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

use super::resolve::resolve_status_at_depth;
use crate::{
    federation::dispatch::FederationError,
    jobs::{AttemptInfo, HandlerFuture, JobError},
    models::status::{self, Status},
    state::AppState,
};

/// The failure budget of a `federator.backfill` job: three tries, then the
/// thread stays as stored — Pleroma's `RemoteFetcherWorker` gives up on a
/// parent the same way.
pub const BACKFILL_MAX_ATTEMPTS: i32 = 3;
/// The unique-key prefix of a backfill job, followed by the child status'
/// id: at most one live backfill per status, the delivery ledger's shape.
const BACKFILL_KEY_PREFIX: &str = "backfill:";

/// The [`HandlerFn`](crate::jobs::HandlerFn) `crate::serve` binds to
/// [`KIND_BACKFILL`](super::KIND_BACKFILL).
pub fn handle(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move { handle_backfill(state, args).await })
}

/// The `federator.backfill` handler: closes one deferred reply link.
///
/// It resolves the parent the status answers and links the child to it. A
/// parent fetched here is ingested at `depth + 1`, which queues the parent's
/// own backfill at that depth.
///
/// `args` is the object [`crate::services::ingest`] enqueued:
/// `{"status_id": "<uuid>", "depth": <n>}`, `depth` being the status'
/// distance from the reply that started the walk; a missing or broken
/// `depth` reads as the first walk.
///
/// # Errors
///
/// [`JobError::Discard`] for args no enqueue should have built or for a
/// parent the peer refuses as unparseable; [`JobError::Retry`] for a
/// database or network failure that may pass on a later attempt. A parent
/// that is gone (`Ok(None)` from [`resolve_status_at_depth`]) or a walk that reaches
/// `THREAD_FETCH_DEPTH` is a quiet completion.
pub async fn handle_backfill(state: AppState, args: Value) -> Result<(), JobError> {
    let Some(raw_id) = args.get("status_id").and_then(Value::as_str) else {
        return Err(JobError::Discard(
            "unusable backfill args: no status id".to_owned(),
        ));
    };
    let Ok(status_id) = Uuid::parse_str(raw_id) else {
        return Err(JobError::Discard(format!(
            "unusable backfill args: `{raw_id}` is not a status id"
        )));
    };
    let depth = args
        .get("depth")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    backfill_one(&state, status_id, depth).await
}

/// One backfill walk: closes the child's deferred parent link. A child
/// that is gone, or whose link a previous pass already closed, is complete.
/// The walk continues up the chain through the parent's own job, which the
/// parent's ingest queued at `depth + 1`.
async fn backfill_one(state: &AppState, child_id: Uuid, depth: i64) -> Result<(), JobError> {
    let child = status::get_by_id(&state.db, child_id)
        .await
        .map_err(|err| retry("the status read", &err))?;
    let Some(child) = child else {
        debug!(%child_id, "the status a backfill was queued for is gone");
        return Ok(());
    };
    if child.in_reply_to_id.is_some() {
        return Ok(());
    }
    resolve_deferred_parent(state, &child, depth).await
}

/// The deferred arm of [`backfill_one`]: a child whose parent link is an
/// id only. Past [`FetchLimits::thread_fetch_depth`] ancestors the walk stops
/// quietly; otherwise [`resolve_status_at_depth`] answers the parent — a
/// freshly fetched one is ingested at `depth + 1`, which is the depth its
/// own backfill job is queued at — and [`link_backfilled_reply`] closes the
/// link and steps the parent's `replies_count` — idempotent, so a retry of a
/// half-finished pass links nothing twice.
///
/// [`link_backfilled_reply`]: crate::models::status::link_backfilled_reply
/// [`FetchLimits::thread_fetch_depth`]: crate::config::FetchLimits::thread_fetch_depth
async fn resolve_deferred_parent(
    state: &AppState,
    child: &Status,
    depth: i64,
) -> Result<(), JobError> {
    let limit = i64::try_from(state.config.fetch.limits.thread_fetch_depth).unwrap_or(i64::MAX);
    if depth >= limit {
        debug!(
            status = %child.id,
            depth, limit, "the thread backfill stops at its depth limit"
        );
        return Ok(());
    }
    let Some(parent_uri) = child.in_reply_to_ap_id.as_deref() else {
        return Ok(());
    };
    let parent = match resolve_status_at_depth(state, parent_uri, depth.saturating_add(1)).await {
        Ok(Some(parent)) => parent,
        Ok(None) => {
            debug!(
                status = %child.id,
                parent = %parent_uri,
                "the answered object is gone or unfoldable; the thread stays as stored"
            );
            return Ok(());
        }
        Err(FederationError::Transient(detail)) => return Err(JobError::Retry(detail)),
        Err(FederationError::Permanent(detail)) => return Err(JobError::Discard(detail)),
    };
    let linked = status::link_backfilled_reply(&state.db, child.id, &parent)
        .await
        .map_err(|err| retry("the reply link write", &err))?;
    if linked {
        debug!(status = %child.id, parent = %parent.id, "a backfilled thread link closed");
    }
    Ok(())
}

/// The dedupe key of the backfill job for the status `status_id`: one
/// live job per deferred reply, queued by ingest.
#[must_use]
pub fn backfill_key(status_id: Uuid) -> String {
    format!("{BACKFILL_KEY_PREFIX}{status_id}")
}

/// Shapes a database failure into the job's retry arm, naming the read or
/// write that failed.
fn retry(what: &str, err: &impl fmt::Display) -> JobError {
    JobError::Retry(format!("{what} failed: {err}"))
}
