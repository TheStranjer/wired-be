//! The status endpoints: `POST /api/v1/statuses`, the single read
//! `GET /api/v1/statuses/{id}` and the batch lookup
//! `GET /api/v1/statuses?id[]=a&id[]=b`
//! (`docs/ARCHITECTURE.md` decision 1).
//!
//! The reference is Pleroma's `StatusController.create`, `.show` and
//! `.index` (`lib/pleroma/web/mastodon_api/controllers/status_controller.ex`).
//! The two reads reduce to the same two moves of their `with` blocks: find
//! the activity, ask `Visibility.visible_for_user?`, and either render it or
//! answer `{:error, :not_found}`. Here finding and asking live in the shared
//! [`status_for_viewer_or_404`], which every later
//! `/api/v1/statuses/{id}/...` route reuses, and rendering lives in the
//! batched page renderer [`crate::services::render_status`].
//!
//! The write is the module's other half, and Pleroma's `create`: a user
//! token granting `write:statuses` (its own scope, not the root) on the
//! `statuses_actions` throttle, the body read through
//! [`ApiBody`] so JSON and form parameters arrive the same way, and the
//! whole decision — what the text says, who it mentions, what it tags, how
//! visible it is and what the author's settings default to — delegated to
//! [`crate::services::create_status`]. The handler adds nothing but the
//! gate, the client behind the token and the JSON envelope, and a post that
//! passes answers **200** with the [`Status`] entity rendered for its
//! author. Everything a fresh status still owes the streaming sockets, the
//! notifications and the wider federation hangs off that service's single
//! documented hook, so this file never grows a second write path.
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
//! The two reads owe no 422: they answer an unusable id with the show
//! route's 404 and the batch route's silent skip, so there is no request
//! either can call invalid, and neither pages (the batch is the requested
//! ids, in order, capped at [`MAX_STATUS_IDS`]). The write route does owe
//! them: every refusal [`crate::services::create_status`] documents is its
//! 422, on top of the extractor's 401 and the scope gate's 403.
//!
//! [`status_for_viewer_or_404`]: crate::api::helpers::status_for_viewer_or_404
//! [`Status`]: crate::entities::status::Status
//! [`ApiBody`]: crate::extract::ApiBody

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::Uri,
    routing::{get, post},
};
use uuid::Uuid;

use crate::{
    api::helpers::{requested_status_ids, status_for_viewer_or_404},
    auth::{AuthUser, MaybeAuthUser},
    entities::status::Status,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        oauth_app::{self, OauthApp},
        status, status_visibility,
    },
    oauth::scopes::{READ_STATUSES, WRITE_STATUSES},
    services::{
        create_status::{self, CreateStatusParams},
        render_status::{render_status, render_statuses},
    },
    state::AppState,
};

/// How many ids one batch lookup acts on: Pleroma's `index` `limit`, with
/// every id past it ignored rather than refused.
pub const MAX_STATUS_IDS: usize = 100;

/// The `/api/v1` statuses router, merged into [`crate::api::v1`].
///
/// `state` is what lets the write route mount the `statuses_actions` rate
/// limiter (see [`crate::rate_limit`]) — the bucket Pleroma's
/// `@rate_limited_status_actions` puts `create` in, shared with
/// [`crate::api::reports`] — while the two read routes stay unthrottled.
pub fn router(state: &AppState) -> Router<AppState> {
    let create =
        post(create_status).layer(crate::rate_limit::rate_limit(state, "statuses_actions"));
    Router::new()
        .route("/statuses/{id}", get(show_status))
        .route("/statuses", get(list_statuses).merge(create))
}

/// `POST /api/v1/statuses` — create the status the body describes and answer
/// its [`Status`] entity as its author sees it.
///
/// The gate is Pleroma's pipeline for the `create` action:
/// [`AuthUser`] then `write:statuses` then the `statuses_actions` throttle
/// (mounted by [`router`]). Everything after that is
/// [`create_status::create_status`]: the parameters, their defaults and
/// their refusals, the render, the mentions and hashtags, the stored row and
/// the post-write hook. The client behind the presenting token is looked up
/// here because only the request knows it, and it travels to the service as
/// the status's `application` pair.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:statuses` check; [`AppError::RateLimited`] from the
/// bucket; whatever [`create_status::create_status`] refuses — 422 for an
/// empty text, an over-long status, an over-long warning, an unknown
/// `visibility` or an unknown `content_type` — plus its own database
/// failures as [`AppError::Database`].
async fn create_status(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<CreateStatusParams>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_STATUSES)?;
    let app: Option<OauthApp> = oauth_app::get_by_id(&state.db, auth.1.app_id).await?;
    Ok(Json(
        create_status::create_status(&state, &auth.0, app.as_ref(), params).await?,
    ))
}

/// `GET /api/v1/statuses/{id}` — the status `id` names, as the viewer
/// behind an optional token may see it (see
/// [`status_for_viewer_or_404`] for what that admits) rendered as the
/// [`Status`] entity.
///
/// # Errors
///
/// [`AppError::NotFound`] for a malformed, unknown, deleted or invisible
/// id, and for the one row a render cannot answer (a reblog whose original
/// went away between the visibility read and the batch load);
/// [`AppError::Forbidden`] when a presented user token lacks
/// `read:statuses`; [`AppError::Unauthorized`] from the extractor for a
/// token that does not resolve. Any database failure arrives through `?`
/// as [`AppError::Database`].
async fn show_status(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require_user_scope(READ_STATUSES)?;
    let viewer = auth.0.as_ref();
    let row = status_for_viewer_or_404(&state, viewer, &id).await?;
    let rendered = render_status(&state, viewer, &row).await?;
    Ok(Json(rendered.ok_or(AppError::NotFound)?))
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
async fn list_statuses(
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
    Ok(Json(render_statuses(&state, viewer, &rows).await?))
}

/// The `visible` rows in the order `requested` names them, each id answered
/// at most once: an id with no row here (unknown, deleted, invisible)
/// contributes nothing, and one named twice contributes one entry, which is
/// what Pleroma's `WHERE id IN (…)` read does for a duplicated id.
fn in_requested_order(requested: &[Uuid], visible: Vec<status::Status>) -> Vec<status::Status> {
    let mut by_id: HashMap<Uuid, status::Status> =
        visible.into_iter().map(|row| (row.id, row)).collect();
    requested.iter().filter_map(|id| by_id.remove(id)).collect()
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::{config::TEST_BASE_URL, models::status::Visibility, util::time::parse_time};

    /// The instant every fixture row in these tests is stamped with.
    fn stamped() -> DateTime<Utc> {
        parse_time("2024-01-31T12:34:56.789Z").expect("the fixture timestamp parses")
    }

    /// A stored row whose only fact of interest is its `id`, so the
    /// ordering helper is tested on the ordering and nothing else.
    fn row(id: Uuid) -> status::Status {
        status::Status {
            id,
            uri: format!("{TEST_BASE_URL}/objects/{id}"),
            url: None,
            user_id: Uuid::nil(),
            content: String::new(),
            raw_text: String::new(),
            content_type: status::DEFAULT_CONTENT_TYPE.to_owned(),
            spoiler_text: String::new(),
            sensitive: false,
            visibility: Visibility::Public,
            language: None,
            in_reply_to_id: None,
            in_reply_to_account_id: None,
            in_reply_to_ap_id: None,
            conversation_id: id,
            reblog_of_id: None,
            quote_of_id: None,
            local: true,
            application_name: None,
            application_website: None,
            replies_count: 0,
            reblogs_count: 0,
            favourites_count: 0,
            quotes_count: 0,
            edited_at: None,
            deleted_at: None,
            expires_at: None,
            inserted_at: stamped(),
            updated_at: stamped(),
        }
    }

    /// The ids of `rows`, in order.
    fn ids(rows: &[status::Status]) -> Vec<Uuid> {
        rows.iter().map(|row| row.id).collect()
    }

    #[test]
    fn the_answer_follows_the_request_not_the_row_order() {
        let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let visible = vec![row(b), row(c), row(a)];
        assert_eq!(
            ids(&in_requested_order(&[a, b, c], visible)),
            vec![a, b, c],
            "rows the batch read answered in id order come back in the \
             order they were asked for"
        );
    }

    #[test]
    fn unknown_ids_drop_out_and_duplicates_answer_once() {
        let (a, b, gone) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let visible = vec![row(b), row(a)];
        assert_eq!(
            ids(&in_requested_order(&[gone, b, a, a], visible)),
            vec![b, a],
            "an id with no visible row contributes nothing, and one asked \
             twice contributes one entry"
        );
    }

    #[test]
    fn nothing_visible_is_the_empty_page() {
        let requested = [Uuid::now_v7()];
        assert!(
            in_requested_order(&requested, Vec::new()).is_empty(),
            "a request whose every id was skipped answers the empty page"
        );
    }
}
