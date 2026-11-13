//! Pleroma's federation status: `GET /api/v1/pleroma/federation_status`
//! (`docs/ARCHITECTURE.md` decision 1, WBE-164).
//!
//! The reference is the `federation_status` action — `show` in
//! `Pleroma.Web.PleromaAPI.InstancesController`
//! (`lib/pleroma/web/pleroma_api/controllers/instances_controller.ex`) —
//! which renders `Pleroma.Instances.get_unreachable/0`, the ledger of
//! remote hosts this instance has marked unreachable, as the object
//! `{"unreachable": {"<host>": "<since>", ...}}`. This endpoint serves the
//! same ledger through [`crate::models::instance::list_unreachable`]:
//! every host carrying an `unreachable_since` mark, oldest mark first, and
//! nothing else — a host merely seen, or one recovered by
//! [`set_reachable`](crate::models::instance::set_reachable), is absent.
//! The grace window
//! ([`UNREACHABLE_GRACE_SECS`](crate::config::Config::unreachable_grace_secs))
//! gates the publisher's deliveries, not this read; Pleroma shows what is
//! marked, not what is currently skipped, and neither does this endpoint
//! apply the window.
//!
//! The `since` value is formatted Mastodon-style — UTC, millisecond
//! precision, a literal `Z` (decision 5) — through
//! [`crate::util::time::mastodon_time`]. Pleroma's own rendering is
//! `to_string(date)`, the raw `NaiveDateTime` text; the ticket fixes the
//! format to this project's convention instead.
//!
//! No authentication and no scopes: Pleroma routes the action on the
//! `:pleroma_api` pipeline, behind no OAuth plug, and every caller gets
//! 200 — the body only names hosts this instance itself failed to reach.

use axum::{Json, Router, extract::State, routing::get};
use serde_json::{Map, Value, json};

use crate::{error::AppResult, models::instance, state::AppState, util::time::mastodon_time};

/// The `/api/v1/pleroma` federation-status router, merged into
/// [`crate::api::v1`] beside the other `/pleroma` mounts.
pub fn router() -> Router<AppState> {
    Router::new().route("/pleroma/federation_status", get(show))
}

/// `GET /api/v1/pleroma/federation_status` — the unreachable-host ledger
/// as `{"unreachable": {"<host>": "<ISO 8601 since>", ...}}`, the empty
/// object under `"unreachable"` while no host is marked. No authentication.
///
/// # Errors
///
/// Any database failure arrives through `?` as
/// [`AppError::Database`](crate::error::AppError).
async fn show(State(state): State<AppState>) -> AppResult<Json<Value>> {
    let marked = instance::list_unreachable(&state.db).await?;
    let unreachable = marked
        .into_iter()
        .map(|(host, since)| (host, Value::String(mastodon_time(&since))))
        .collect::<Map<String, Value>>();
    Ok(Json(json!({ "unreachable": unreachable })))
}
