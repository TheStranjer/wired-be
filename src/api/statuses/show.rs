//! # The reads
//!
//! `GET /api/v1/statuses/{id}` is Pleroma's `show`: authentication is
//! optional ([`MaybeAuthUser`], with `read:statuses` enforced on a
//! presented user token through
//! [`require_user_scope`](MaybeAuthUser::require_user_scope)) and the answer
//! is the [`Status`] entity as `viewer` sees it. A malformed id, an id that
//! names no row, a deleted row and a row the visibility model hides from
//! this viewer are one indistinguishable 404
//! `{"error":"Record not found"}`, so the endpoint never tells a stranger
//! that somebody's private post exists. A reblog's id answers the *reblog*
//! entity — the reblogger's own row, with the original nested in `reblog`
//! — which is what the `Announce` arm of Pleroma's `show.json` renders.
//!
//! The batch route is Pleroma's `index`: the same optional auth and scope,
//! the ids read by [`requested_status_ids`] (`id[]`, and the deprecated
//! `ids[]` alias, both also accepted unwrapped), and the answer an array of
//! [`Status`] entities for the ids this viewer may see *in the order they
//! were requested*. Ids that are malformed, unknown, deleted or invisible
//! contribute nothing and are never an error, a request that carries no
//! usable id is the empty array, and only the first [`MAX_STATUS_IDS`] ids
//! are acted on — Pleroma's `limit = 100` over `Enum.take(ids, limit)`,
//! which drops the rest silently rather than refusing the call.
//!
//! The card read is Mastodon's deprecated `GET /api/v1/statuses/{id}/card`,
//! which Pleroma keeps: the same optional auth and scope as the show route
//! and the same [`status_for_viewer_or_404`] gate, and the answer the
//! status's stored rich-media card — the same cached JSON the batched
//! renderer emits into the entity's `card` key — or, for a status that
//! links no card, JSON `null` with 200: having no card is an answer, not
//! a miss, so only the 404 gate can refuse this read.
//!
//! [`Status`]: crate::entities::status::Status
//! [`status_for_viewer_or_404`]: crate::api::helpers::status_for_viewer_or_404

use axum::{
    Json,
    extract::{Path, State},
    http::Uri,
};
use serde_json::Value;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::{
        helpers::{QueryPairs, is_true, requested_status_ids, status_for_viewer_or_404},
        pleroma_account_settings::looks_like_ap_id,
    },
    auth::MaybeAuthUser,
    entities::status::Status,
    error::{AppError, AppResult},
    federation::fetch_object::resolve_status,
    models::{
        link_card,
        status::{self, Status as StatusRow},
        status_visibility,
        user::User,
    },
    oauth::scopes::READ_STATUSES,
    services::{
        render_status::{ViewerMode, render_status, render_statuses},
        rich_media::stored_card_json,
    },
    state::AppState,
};

use super::{MAX_STATUS_IDS, in_requested_order};

/// `GET /api/v1/statuses/{id}` — the status `id` names, as the viewer
/// behind an optional token may see it (see
/// [`status_for_viewer_or_404`] for what that admits) rendered as the
/// [`Status`] entity.
///
/// A `UUID` miss has one second life: with `resolve=true` and a
/// [`looks_like_ap_id`] `{id}`, the segment is an object URI and
/// [`status_by_uri_or_404`] resolves it over federation. Resolution needs
/// a viewer (the search's rule — a federation fetch runs on somebody's
/// behalf), and every miss in it is this route's one 404 too.
///
/// # Errors
///
/// [`AppError::NotFound`] for a malformed, unknown, deleted or invisible
/// id — including a `resolve=true` URI nothing resolves to — and for the
/// one row a render cannot answer (a reblog whose original went away
/// between the visibility read and the batch load);
/// [`AppError::Forbidden`] when a presented user token lacks
/// `read:statuses`; [`AppError::Unauthorized`] from the extractor for a
/// token that does not resolve. Any database failure arrives through `?`
/// as [`AppError::Database`].
pub(super) async fn show_status(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    uri: Uri,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require_user_scope(READ_STATUSES)?;
    let viewer = auth.0.as_ref();
    let row = match status_for_viewer_or_404(&state, viewer, &id).await {
        Ok(row) => row,
        Err(AppError::NotFound)
            if viewer.is_some() && wants_resolution(&uri) && looks_like_ap_id(&id) =>
        {
            status_by_uri_or_404(&state, viewer, &id).await?
        }
        Err(err) => return Err(err),
    };
    let rendered = render_status(&state, ViewerMode::User(viewer), &row).await?;
    Ok(Json(rendered.ok_or(AppError::NotFound)?))
}

/// Whether this request asks for federation resolution: a `resolve`
/// parameter spelling `true` (the [`crate::api::search`] routes' reading).
fn wants_resolution(uri: &Uri) -> bool {
    QueryPairs::parse(uri.query())
        .value("resolve")
        .is_some_and(|value| is_true(&value))
}

/// The `resolve=true` arm of [`show_status`]: `id` is an object URI,
/// [`resolve_status`] dereferences and folds it (a stored row answers
/// first, without the network), and what resolves must still pass the
/// show route's own visibility test for this viewer. A resolution the
/// federation cannot answer — refused, gone, no `Note` behind it — or a
/// folded row the viewer may not see is the module's one 404; the
/// resolver's reason is logged, never sent (decision 8).
///
/// # Errors
///
/// [`AppError::NotFound`] as described; any database failure arrives
/// through `?` as [`AppError::Database`].
async fn status_by_uri_or_404(
    state: &AppState,
    viewer: Option<&User>,
    id: &str,
) -> AppResult<StatusRow> {
    let found = match resolve_status(state, id).await {
        Ok(Some(row)) => row,
        Ok(None) => return Err(AppError::NotFound),
        Err(err) => {
            debug!(%err, uri = %id, "a status show could not resolve the requested uri");
            return Err(AppError::NotFound);
        }
    };
    if status_visibility::can_view(&state.db, &found, viewer).await? {
        Ok(found)
    } else {
        Err(AppError::NotFound)
    }
}

/// `GET /api/v1/statuses/{id}/card` — the stored rich-media card of the
/// status `id`, or JSON `null` when it has none (Mastodon's deprecated
/// endpoint, which Pleroma keeps).
///
/// The gate is the show route's: optional auth ([`MaybeAuthUser`], with
/// `read:statuses` enforced on a presented user token) and the shared
/// [`status_for_viewer_or_404`], so a malformed, unknown, deleted or
/// invisible id is the module's one 404 and never a `null` that hints at
/// an existing private post. Behind it one [`link_card`] read by the
/// status's own id answers the row the fetch job linked; [`stored_card_json`]
/// renders its cached [`card_json`] output verbatim — the same reading
/// the entity's `card` key uses — and a status that links no card, or
/// links only a failed fetch, answers JSON `null` with 200.
///
/// [`link_card`]: crate::models::link_card
/// [`card_json`]: crate::services::rich_media::card_json
///
/// # Errors
///
/// [`AppError::NotFound`] for a malformed, unknown, deleted or invisible
/// id; [`AppError::Forbidden`] when a presented user token lacks
/// `read:statuses`; [`AppError::Unauthorized`] from the extractor for a
/// token that does not resolve. Any database failure arrives through `?`
/// as [`AppError::Database`].
pub(super) async fn get_status_card(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    auth.require_user_scope(READ_STATUSES)?;
    let row = status_for_viewer_or_404(&state, auth.0.as_ref(), &id).await?;
    let cards = link_card::for_statuses(&state.db, &[row.id]).await?;
    debug!(
        status = %row.id,
        linked = cards.contains_key(&row.id),
        "status card read"
    );
    Ok(Json(
        cards
            .get(&row.id)
            .and_then(stored_card_json)
            .unwrap_or(Value::Null),
    ))
}

/// `GET /api/v1/statuses?id[]=a&id[]=b` (or `ids[]=`) — the [`Status`]
/// entities of the requested ids this viewer may see, in the requested
/// order, with the malformed, unknown, deleted and invisible ids skipped.
/// No usable id at all is the empty array, as Pleroma's `index` answers it
/// for `ids = []`; only the first [`MAX_STATUS_IDS`] ids are looked up.
///
/// # Errors
///
/// [`AppError::Forbidden`] when a presented user token lacks
/// `read:statuses`; [`AppError::Unauthorized`] from the extractor for a
/// token that does not resolve. Any database failure arrives through `?`
/// as [`AppError::Database`].
pub(super) async fn list_statuses(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    uri: Uri,
) -> AppResult<Json<Vec<Status>>> {
    auth.require_user_scope(READ_STATUSES)?;
    let viewer = auth.0.as_ref();
    let requested: Vec<Uuid> = requested_status_ids(uri.query())
        .into_iter()
        .take(MAX_STATUS_IDS)
        .collect();
    if requested.is_empty() {
        return Ok(Json(Vec::new()));
    }
    // One batch read, one batch visibility clause, then the page is put
    // back into the order the client asked in (`get_many` answers id order,
    // `filter_visible` keeps whatever order it was handed).
    let live = status::get_many(&state.db, &requested).await?;
    let visible = status_visibility::filter_visible(&state.db, live, viewer).await?;
    let rows = in_requested_order(&requested, visible);
    Ok(Json(
        render_statuses(&state, ViewerMode::User(viewer), &rows).await?,
    ))
}
