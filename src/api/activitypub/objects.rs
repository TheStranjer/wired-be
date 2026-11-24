//! The object `GET`s of the `ActivityPub` router: `GET /objects/{uuid}`
//! and its nickname-scoped twin, `GET /notice/{id}`, and an object's
//! replies collection. The three status reads share one core
//! ([`object_core`]); the tombstone, the author check and the visibility
//! admission ([`read_admitted`]) beside it are the same gates the
//! [`crate::federation::note`] documents are served under.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse as _, Response},
};
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    federation::{
        authorized_fetch::{fetch_allowed, fetch_signer},
        collections,
        note::{load_note_context, note_json, tombstone_json},
    },
    models::{
        status::{self, Status, Visibility},
        status_visibility,
        user::get_by_nickname,
    },
    state::AppState,
    util::id::parse_id,
};

use super::ACTIVITY_JSON;
use super::negotiation::{
    document, document_response, frontend_redirect, wants_activity_json, wants_html, with_vary,
};

/// `GET /objects/{uuid}/replies` — the replies collection of one local
/// status: the `Collection` root with the count of publicly visible
/// answers, or with `?page=true` a `CollectionPage` of their object ids,
/// oldest first, up to 20 per page. An unknown, tombstoned, non-local or
/// not-publicly-visible object answers the same 404 as an unknown actor.
pub(super) async fn get_replies(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(raw_id): Path<String>,
) -> Response {
    let response = object_replies_answer(&state, &raw_id, &headers, &uri).await;
    with_vary(response)
}

/// The replies route's answer: the same three-way negotiation as the
/// actor route (a browser leaves for the frontend post page, which shows
/// the thread), then the gate, the object, and its document.
async fn object_replies_answer(
    state: &AppState,
    raw_id: &str,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<Response> {
    if !wants_activity_json(headers) {
        if wants_html(headers) {
            return frontend_redirect(&state.config, &format!("notice/{raw_id}"));
        }
        return Err(AppError::RouteNotFound);
    }
    fetch_allowed(state, &Method::GET, uri, headers).await?;
    let object = publicly_replyable(state, raw_id).await?;
    let built = collections::replies_document(&state.db, &state.config, &object, uri.query()).await;
    document(built)
}

/// The object behind `GET /objects/{uuid}/replies`: the id must parse,
/// the row must be a live status this instance authored, and it must be
/// visible to the anonymous reader (Pleroma's `visible_for_user?` gate of
/// `object_replies`, which a `private`, `direct` or `local` post fails) —
/// every miss the one 404.
async fn publicly_replyable(state: &AppState, raw_id: &str) -> AppResult<Status> {
    let missing = || AppError::RouteNotFound;
    let id = parse_id(raw_id).ok_or_else(missing)?;
    let object = status::get_by_id(&state.db, id)
        .await?
        .filter(|row| row.local)
        .ok_or_else(missing)?;
    status_visibility::require_visible(&state.db, object, None)
        .await
        .map_err(|err| {
            if matches!(err, AppError::NotFound) {
                AppError::RouteNotFound
            } else {
                err
            }
        })
}

/// `GET /objects/{uuid}` — the status document with the `ActivityPub`
/// object id `{BASE_URL}/objects/{uuid}` (decision 6), whatever `Accept`
/// names an `ActivityPub` media type; anything else is the plain 404. No
/// OAuth: peers dereference objects anonymously — or, once
/// `AUTHORIZED_FETCH` is on, with HTTP Signatures.
pub(super) async fn get_object(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(uuid): Path<String>,
) -> Response {
    let outcome = object_core(&state, &uuid, None, &headers, &uri).await;
    with_vary(outcome)
}

/// `GET /users/{nickname}/statuses/{uuid}` — the same document as
/// [`get_object`], reached through the nickname-scoped path Pleroma
/// answers with the shared `object/2` action: the named account must
/// actually be the status' live local author, or the read is the plain 404.
pub(super) async fn get_scoped_object(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path((nickname, uuid)): Path<(String, String)>,
) -> Response {
    let outcome = object_core(&state, &uuid, Some(&nickname), &headers, &uri).await;
    with_vary(outcome)
}

/// `GET /notice/{id}` — the human-facing post URL whose `{id}` is the
/// status uuid the model writes into [`Status::url`]. An `Accept` naming
/// an `ActivityPub` media type is a federating client and gets the very
/// document [`get_object`] serves; an `Accept` naming `text/html` (and no
/// `ActivityPub` type) is a browser and gets the 302 to the same path on
/// the frontend origin (see [`frontend_redirect`]); anything else, the 404.
pub(super) async fn get_notice(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(id): Path<String>,
) -> Response {
    let outcome = if wants_activity_json(&headers) {
        object_core(&state, &id, None, &headers, &uri).await
    } else if wants_html(&headers) {
        frontend_redirect(&state.config, &format!("notice/{id}"))
    } else {
        Err(AppError::RouteNotFound)
    };
    with_vary(outcome)
}

/// The shared object-read core of the three routes above, the steps in
/// the order the ticket fixes them: an unparsable uuid is the 404 (the
/// route cannot name a document it cannot address); only `ActivityPub`
/// clients get documents here — unlike the actor route, no browser is
/// redirected; then the `AUTHORIZED_FETCH` gate, in the same position
/// [`gate_actor_read`](super::gate_actor_read) runs it, so an unsigned read under
/// the gate is the
/// 401 before any status is read; then the row, which must be a live
/// original this instance owns (no remote mirror, no reblog row — an
/// `Announce` is served by its `/activities/{uuid}` id); then, for the
/// nickname-scoped twin, the author check; a soft-deleted row answers 410
/// with its `Tombstone` document; `local` visibility is never an `ActivityPub`
/// document, and `private`/`direct` are served only to a signature the
/// gate proves *and* [`status_visibility::can_view`] admits; and the
/// survivor is rendered by [`note_json`].
async fn object_core(
    state: &AppState,
    id_text: &str,
    nickname: Option<&str>,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<Response> {
    let Some(id) = Uuid::parse_str(id_text).ok() else {
        return Err(AppError::RouteNotFound);
    };
    if !wants_activity_json(headers) {
        return Err(AppError::RouteNotFound);
    }
    fetch_allowed(state, &Method::GET, uri, headers).await?;
    let Some(status) = status::get_by_id_including_deleted(&state.db, id).await? else {
        return Err(AppError::RouteNotFound);
    };
    if !status.local || status.reblog_of_id.is_some() {
        return Err(AppError::RouteNotFound);
    }
    if !nickname_owns(state, nickname, &status).await? {
        return Err(AppError::RouteNotFound);
    }
    if status.deleted_at.is_some() {
        return Ok(tombstone_response(&status));
    }
    if !read_admitted(state, &status, headers, uri).await? {
        return Err(AppError::RouteNotFound);
    }
    let context = load_note_context(&state.db, &state.config, status.id).await?;
    let Some(note) = note_json(&context) else {
        // Unreachable: `note_json` is `None` only for `local`, which the
        // check above already refused. A document this route cannot render
        // is the same 404 as one it cannot find.
        return Err(AppError::RouteNotFound);
    };
    Ok(document_response(&note))
}

/// Whether `nickname` names the status' author: the live local account it
/// names must be the one who wrote the row. `None` — the unscoped paths —
/// always owns the read; an unknown, remote, deactivated or different
/// author is a `false`, and the caller answers the plain 404 either way.
pub(super) async fn nickname_owns(
    state: &AppState,
    nickname: Option<&str>,
    status: &Status,
) -> AppResult<bool> {
    let Some(nickname) = nickname else {
        return Ok(true);
    };
    let owner = get_by_nickname(&state.db, nickname).await?;
    Ok(owner.is_some_and(|owner| owner.id == status.user_id && owner.is_local_active()))
}

/// Whether this read is admitted. `local` is never federated and answers
/// nobody; every other visibility is [`status_visibility::can_view`]'s
/// decision, made for the signer `fetch_signer` proves (or for no signer,
/// when `AUTHORIZED_FETCH` is off and the request is unsigned). So a blocked
/// signer or a deactivated author is refused even for a public post.
/// A signature that fails to verify is `fetch_signer`'s 401, not a 404:
/// the requester claimed an identity and could not back it.
pub(super) async fn read_admitted(
    state: &AppState,
    status: &Status,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<bool> {
    if status.visibility == Visibility::Local {
        return Ok(false);
    }
    let signer = fetch_signer(state, &Method::GET, uri, headers).await?;
    status_visibility::can_view(&state.db, status, signer.as_ref()).await
}

/// The `Tombstone` a soft-deleted status is served as: the 410 whose body
/// is [`tombstone_json`]'s document. Built by hand rather than through
/// [`AppError::Gone`], which would answer `{"error":"Gone"}` where a
/// federating reader expects the tombstone.
fn tombstone_response(status: &Status) -> Response {
    (
        StatusCode::GONE,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(ACTIVITY_JSON),
        )],
        Json(tombstone_json(status)),
    )
        .into_response()
}
