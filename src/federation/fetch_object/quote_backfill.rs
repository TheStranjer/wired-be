//! The `federator.quote_backfill` job: fetch the object an unlinked remote
//! quote names and link the quote to it.

use std::fmt;

use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

use super::resolve::resolve_quoted_status;
use crate::{
    federation::dispatch::FederationError,
    jobs::{AttemptInfo, HandlerFuture, JobError},
    models::status,
    services::notify,
    state::AppState,
};

/// The failure budget of a `federator.quote_backfill` job: three tries,
/// then the quote stays a plain post — the reply backfill's budget.
pub const QUOTE_BACKFILL_MAX_ATTEMPTS: i32 = 3;
/// The unique-key prefix of a quote backfill job, followed by the quoting
/// status' id: at most one live job per unlinked quote.
const QUOTE_BACKFILL_KEY_PREFIX: &str = "quote_backfill:";

/// The [`HandlerFn`](crate::jobs::HandlerFn) `crate::serve` binds to
/// [`KIND_QUOTE_BACKFILL`](super::KIND_QUOTE_BACKFILL).
pub fn handle_quote(state: AppState, _attempt: AttemptInfo, args: Value) -> HandlerFuture {
    Box::pin(async move { handle_quote_backfill(state, args).await })
}

/// The `federator.quote_backfill` handler: links one unlinked quote.
///
/// `args` is `{"status_id": "<uuid>"}`, the quoting status. The quoted
/// object is resolved through the object fetcher under one fetch budget, and
/// ingested without queueing a quote or reply backfill of its own, so a quote
/// of a quote is never chased. A quote that is gone, tombstoned or already
/// linked, a quoted object that is gone (404/410), not a note, or refused by
/// the ingest gate, all complete quietly and leave the quote a plain post.
///
/// # Errors
///
/// [`JobError::Discard`] for args no enqueue should have built or a quoted
/// object the peer serves unparseably; [`JobError::Retry`] for a database or
/// network failure that may pass on a later attempt.
pub async fn handle_quote_backfill(state: AppState, args: Value) -> Result<(), JobError> {
    let Some(raw_id) = args.get("status_id").and_then(Value::as_str) else {
        return Err(JobError::Discard(
            "unusable quote backfill args: no status id".to_owned(),
        ));
    };
    let Ok(status_id) = Uuid::parse_str(raw_id) else {
        return Err(JobError::Discard(format!(
            "unusable quote backfill args: `{raw_id}` is not a status id"
        )));
    };
    link_quote(&state, status_id).await
}

/// One quote's pass: read the reference it waits for, resolve it, link it.
async fn link_quote(state: &AppState, quoting_id: Uuid) -> Result<(), JobError> {
    let pending = status::pending_quote_uri(&state.db, quoting_id)
        .await
        .map_err(|err| retry("the quote reference read", &err))?;
    let Some(quote_uri) = pending else {
        debug!(status = %quoting_id, "the quote needs no fetch: gone, linked or quoting nothing");
        return Ok(());
    };
    let quoted = match resolve_quoted_status(state, &quote_uri).await {
        Ok(Some(quoted)) => quoted,
        Ok(None) => {
            debug!(
                status = %quoting_id,
                quoted = %quote_uri,
                "the quoted object is gone or unfoldable; the quote stays unlinked"
            );
            return Ok(());
        }
        Err(FederationError::Transient(detail)) => return Err(JobError::Retry(detail)),
        Err(FederationError::Permanent(detail)) => return Err(JobError::Discard(detail)),
    };
    let linked = status::link_backfilled_quote(&state.db, quoting_id, &quoted)
        .await
        .map_err(|err| retry("the quote link write", &err))?;
    if linked {
        debug!(status = %quoting_id, quoted = %quoted.id, "a backfilled quote linked");
        notify::on_quote_linked(state, quoting_id).await;
    }
    Ok(())
}

/// The dedupe key of the quote backfill job for the status `status_id`.
#[must_use]
pub fn quote_backfill_key(status_id: Uuid) -> String {
    format!("{QUOTE_BACKFILL_KEY_PREFIX}{status_id}")
}

/// Shapes a database failure into the job's retry arm, naming the read or
/// write that failed.
fn retry(what: &str, err: &impl fmt::Display) -> JobError {
    JobError::Retry(format!("{what} failed: {err}"))
}
