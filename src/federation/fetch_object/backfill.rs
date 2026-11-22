//! The `federator.backfill` job: walk a thread up from a reply whose parent
//! this instance does not have, one link per run.

use std::fmt;

use serde_json::{Value, json};
use tracing::debug;
use uuid::Uuid;

use super::{KIND_BACKFILL, resolve::resolve_status};
use crate::{
    federation::dispatch::FederationError,
    jobs::{AttemptInfo, HandlerFuture, JobError, QUEUE_BACKGROUND},
    models::{
        job::{self, NewJob},
        status::{self, Status},
    },
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
/// [`KIND_BACKFILL`].
pub fn handle(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move { handle_backfill(state, args).await })
}

/// The `federator.backfill` handler: closes one deferred reply link —
/// resolve the parent the status answers, link the child to it, and queue
/// the parent's own unresolved parent at `depth + 1`.
///
/// `args` is the object [`crate::services::ingest`] or this handler
/// enqueued: `{"status_id": "<uuid>", "depth": <n>}`; a missing or broken
/// `depth` reads as the first walk.
///
/// # Errors
///
/// [`JobError::Discard`] for args no enqueue should have built or for a
/// parent the peer refuses as unparseable; [`JobError::Retry`] for a
/// database or network failure that may pass on a later attempt. A parent
/// that is gone (`Ok(None)` from [`resolve_status`]) or a walk that reaches
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

/// One backfill walk: the parent the child answers (`None` — complete —
/// when the child is gone, the link is already closed by a previous pass
/// whose parent vanished, or the deferred parent never resolves to a
/// foldable status), then the walk continues up the chain.
async fn backfill_one(state: &AppState, child_id: Uuid, depth: i64) -> Result<(), JobError> {
    let child = status::get_by_id(&state.db, child_id)
        .await
        .map_err(|err| retry("the status read", &err))?;
    let Some(child) = child else {
        debug!(%child_id, "the status a backfill was queued for is gone");
        return Ok(());
    };
    let parent = match child.in_reply_to_id {
        Some(parent_id) => status::get_by_id(&state.db, parent_id)
            .await
            .map_err(|err| retry("the parent read", &err))?,
        None => resolve_deferred_parent(state, &child, depth).await?,
    };
    match parent {
        Some(parent) => continue_chain(state, &parent, depth).await,
        None => Ok(()),
    }
}

/// The deferred arm of [`backfill_one`]: a child whose parent link is an
/// id only. Past [`FetchLimits::thread_fetch_depth`] ancestors the walk stops
/// quietly; otherwise [`resolve_status`] answers the parent and
/// [`link_backfilled_reply`] closes the link and steps the parent's
/// `replies_count` — idempotent, so a retry of a half-finished pass links
/// nothing twice.
///
/// [`link_backfilled_reply`]: crate::models::status::link_backfilled_reply
/// [`FetchLimits::thread_fetch_depth`]: crate::config::FetchLimits::thread_fetch_depth
async fn resolve_deferred_parent(
    state: &AppState,
    child: &Status,
    depth: i64,
) -> Result<Option<Status>, JobError> {
    let limit = i64::try_from(state.config.fetch.limits.thread_fetch_depth).unwrap_or(i64::MAX);
    if depth >= limit {
        debug!(
            status = %child.id,
            depth, limit, "the thread backfill stops at its depth limit"
        );
        return Ok(None);
    }
    let Some(parent_uri) = child.in_reply_to_ap_id.as_deref() else {
        return Ok(None);
    };
    let parent = match resolve_status(state, parent_uri).await {
        Ok(Some(parent)) => parent,
        Ok(None) => {
            debug!(
                status = %child.id,
                parent = %parent_uri,
                "the answered object is gone or unfoldable; the thread stays as stored"
            );
            return Ok(None);
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
    Ok(Some(parent))
}

/// The chain arm: when the parent *itself* answers an unstored object, its
/// own backfill runs at `depth + 1`. Ingest queued that job at depth `0`
/// the moment the parent's row was created (see
/// [`crate::services::ingest`]) — this walk's depth bookkeeping wins, so
/// the pending pass is cancelled and re-queued one deeper, which is what
/// makes the limit bind on a chain fetched end to end. One step from the
/// limit, the cancelled pass is not re-queued and the walk ends.
async fn continue_chain(state: &AppState, parent: &Status, depth: i64) -> Result<(), JobError> {
    if parent.in_reply_to_ap_id.is_none() {
        return Ok(());
    }
    let unique_key = backfill_key(parent.id);
    // Cancel parent's own queued pass unconditionally when it carries a
    // deferred link: this walk owns the depth bookkeeping from here, so at
    // the limit the pending pass is dropped rather than left to restart the
    // walk uncounted.
    if let Err(err) = job::cancel_pending(&state.db, &unique_key).await {
        debug!(%err, parent = %parent.id, "the pending backfill cancel failed; queueing anyway");
    }
    let limit = i64::try_from(state.config.fetch.limits.thread_fetch_depth).unwrap_or(i64::MAX);
    if depth + 1 >= limit {
        debug!(
            parent = %parent.id,
            depth, limit, "the thread backfill stops at its depth limit"
        );
        return Ok(());
    }
    let args = json!({ "status_id": parent.id.to_string(), "depth": depth + 1 });
    let new_job = NewJob {
        queue: QUEUE_BACKGROUND,
        kind: KIND_BACKFILL,
        args,
        run_at: None,
        max_attempts: Some(BACKFILL_MAX_ATTEMPTS),
        unique_key: Some(&unique_key),
    };
    job::enqueue(&state.db, new_job)
        .await
        .map(drop)
        .map_err(|err| {
            // The child's link is already committed; a lost deeper job costs
            // the chain one pass, and this attempt reports it for retry — the
            // link write reruns as the no-op it is.
            retry("the backfill enqueue", &err)
        })
}

/// The dedupe key of the backfill job for the status `status_id`: one
/// live job per deferred reply, shared by the ingest enqueue and the
/// chain walk.
#[must_use]
pub fn backfill_key(status_id: Uuid) -> String {
    format!("{BACKFILL_KEY_PREFIX}{status_id}")
}

/// Shapes a database failure into the job's retry arm, naming the read or
/// write that failed.
fn retry(what: &str, err: &impl fmt::Display) -> JobError {
    JobError::Retry(format!("{what} failed: {err}"))
}
