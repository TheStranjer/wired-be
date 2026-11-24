//! The activity `GET`s of the `ActivityPub` router: `GET
//! /activities/{uuid}` and the `Create` of a notice,
//! `GET /users/{nickname}/statuses/{uuid}/activity`. The shared core
//! ([`activity_core`]) resolves the row
//! [`find_local_by_id`](crate::models::activity::find_local_by_id) names
//! and renders it through the status builders ([`create_document`],
//! [`announce_document`]) or the relation builders ([`relation_json`]),
//! under the same [`read_admitted`] gate the object reads run.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, Method, Uri},
    response::Response,
};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    federation::{
        activities::{
            EmojiReactContext, ObjectActivityContext, activity_id, block_json, emoji_react_json,
            follow_json, like_json,
        },
        authorized_fetch::{fetch_allowed, fetch_signer},
        note::{announce_activity_json, create_activity_json, load_note_context, note_json},
        vocab::AS_PUBLIC,
    },
    models::{
        activity::{self, LocalActivity, LocalActivityKind},
        status::{self, Status},
        user::get_by_id,
    },
    state::AppState,
};

use super::negotiation::{document_response, wants_activity_json, with_vary};
use super::objects::{nickname_owns, read_admitted};

/// `GET /activities/{uuid}` — the local activity whose row `UUIDv7` names
/// it (Pleroma's `ActivityPubController.activity/2`): the `Create` or
/// `Announce` of a status row, or the `Like`, `EmojiReact`, `Follow` or
/// `Block` behind a relation row, at the `{BASE_URL}/activities/{uuid}` id
/// decision 6 mints. Negotiation and gates as the object reads: `Accept`
/// must name an `ActivityPub` media type, and `AUTHORIZED_FETCH` demands
/// the signature before anything resolves.
pub(super) async fn get_activity(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(uuid): Path<String>,
) -> Response {
    let outcome = activity_core(&state, &uuid, &headers, &uri).await;
    with_vary(outcome)
}

/// `GET /users/{nickname}/statuses/{uuid}/activity` — Pleroma's
/// `notice_activity`: the `Create` activity of that live, non-reblog,
/// local status, the object core's document wrapped. No HTML branch: like
/// the activity route it answers only `ActivityPub` clients.
pub(super) async fn get_notice_activity(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path((nickname, uuid)): Path<(String, String)>,
) -> Response {
    let outcome = notice_activity_core(&state, &uuid, &nickname, &headers, &uri).await;
    with_vary(outcome)
}

/// The shared `GET /activities/{uuid}` core, in the order the ticket fixes
/// it: an `Accept` naming no `ActivityPub` media type is the plain 404, the
/// `AUTHORIZED_FETCH` gate runs next, an unparsable uuid names no row, and
/// an id no activity table carries resolves to nothing. What does resolve
/// is a status arm (`Create`/`Announce`), gated exactly like the object
/// core, or a relation arm (`Like`/`EmojiReact`/`Follow`/`Block`), served
/// only to the activity's two parties over a signature.
async fn activity_core(
    state: &AppState,
    id_text: &str,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<Response> {
    if !wants_activity_json(headers) {
        return Err(AppError::RouteNotFound);
    }
    fetch_allowed(state, &Method::GET, uri, headers).await?;
    let Some(id) = Uuid::parse_str(id_text).ok() else {
        return Err(AppError::RouteNotFound);
    };
    let Some(resolved) = activity::find_local_by_id(&state.db, id).await? else {
        return Err(AppError::RouteNotFound);
    };
    let document = match resolved.kind {
        LocalActivityKind::Create | LocalActivityKind::Announce => {
            status_activity_document(state, &resolved, headers, uri).await?
        }
        LocalActivityKind::Like
        | LocalActivityKind::EmojiReact
        | LocalActivityKind::Follow
        | LocalActivityKind::Block => {
            relation_activity_document(state, &resolved, headers, uri).await?
        }
    };
    Ok(document_response(&document))
}

/// The `Create` or `Announce` behind a resolved status arm. The row goes
/// through the object core's checks on the re-read the gate and the
/// builders work from: a row this instance does not own, a deletion that
/// raced in since the arm resolved (a soft-deleted status never resolves
/// to an arm, and this route answers its id 404, never a `Tombstone`), and
/// a visibility [`read_admitted`] refuses are all the plain 404.
async fn status_activity_document(
    state: &AppState,
    resolved: &LocalActivity,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<Value> {
    let Some(stored) = status::get_by_id_including_deleted(&state.db, resolved.id).await? else {
        return Err(AppError::RouteNotFound);
    };
    if !stored.local || stored.deleted_at.is_some() {
        return Err(AppError::RouteNotFound);
    }
    if !read_admitted(state, &stored, headers, uri).await? {
        return Err(AppError::RouteNotFound);
    }
    if stored.reblog_of_id.is_some() {
        announce_document(state, resolved, &stored).await
    } else {
        create_document(state, &stored).await
    }
}

/// The status' `Create`: the very [`note_json`] document
/// [`object_core`](super::objects::object_core)
/// serves, wrapped by [`create_activity_json`]. Its `id` is
/// `{BASE_URL}/activities/{status id}` (decision 6), exactly
/// [`activity_id`](crate::federation::activities::activity_id) of the row
/// that resolved. [`note_json`]'s `None` is a `local` status, which
/// [`read_admitted`] has just refused, so the unrenderable case is the
/// same 404 as on the object read.
async fn create_document(state: &AppState, stored: &Status) -> AppResult<Value> {
    let context = load_note_context(&state.db, &state.config, stored.id).await?;
    let Some(note) = note_json(&context) else {
        return Err(AppError::RouteNotFound);
    };
    Ok(create_activity_json(&note, stored, &state.config))
}

/// The reblog's `Announce`: the row in hand and its author, naming the
/// boosted original's `uri` and its author's `ap_id` — both carried by the
/// resolved arm, so no second read of the original. The builder's `None`
/// is a `local` reblog, which [`read_admitted`] has just refused.
async fn announce_document(
    state: &AppState,
    resolved: &LocalActivity,
    stored: &Status,
) -> AppResult<Value> {
    let Some(author) = get_by_id(&state.db, stored.user_id).await? else {
        return Err(AppError::RouteNotFound);
    };
    let Some(original_author_ap_id) = resolved.target_ap_id.as_deref() else {
        return Err(AppError::RouteNotFound);
    };
    announce_activity_json(
        stored,
        &author,
        &resolved.object_ap_id,
        original_author_ap_id,
        &state.config,
    )
    .ok_or(AppError::RouteNotFound)
}

/// The `Like`, `EmojiReact`, `Follow` or `Block` behind a resolved
/// relation arm, served only to a signed fetch whose reader is one of the
/// activity's two parties: [`fetch_signer`]'s `None` — the anonymous read
/// with `AUTHORIZED_FETCH` off — is a 404 like any unrelated reader's, and
/// a signature that fails to verify is its 401.
async fn relation_activity_document(
    state: &AppState,
    resolved: &LocalActivity,
    headers: &HeaderMap,
    uri: &Uri,
) -> AppResult<Value> {
    let Some(signer) = fetch_signer(state, &Method::GET, uri, headers).await? else {
        return Err(AppError::RouteNotFound);
    };
    let party = signer.id == resolved.actor_user_id
        || resolved
            .target_user_id
            .is_some_and(|target| target == signer.id);
    if !party {
        return Err(AppError::RouteNotFound);
    }
    relation_json(state, resolved)
}

/// The relation document rendered straight from the resolved row's
/// columns — the ids, the object, the publication moment, and the emoji
/// pair only the `EmojiReact` arm carries — with no further read. The
/// status arms never reach here; should one, its id names no relation
/// document and the plain 404 answers.
fn relation_json(state: &AppState, resolved: &LocalActivity) -> AppResult<Value> {
    let config = &state.config;
    let id = resolved.id;
    let actor = resolved.actor_ap_id.as_str();
    let object = resolved.object_ap_id.as_str();
    let published = &resolved.inserted_at;
    match resolved.kind {
        // The served relation document keeps this module's public-address
        // convention: what the outbound hooks delivered is their own
        // addressing, computed from the acted-on status.
        LocalActivityKind::Like => {
            let to = [AS_PUBLIC];
            let cc: [&str; 0] = [];
            let document = activity_id(config, id);
            Ok(like_json(&ObjectActivityContext {
                config,
                id: &document,
                actor_ap_id: actor,
                object_uri: object,
                to: &to,
                cc: &cc,
                published,
            }))
        }
        LocalActivityKind::EmojiReact => {
            let Some(emoji) = resolved.emoji.as_deref() else {
                return Err(AppError::RouteNotFound);
            };
            let to = [AS_PUBLIC];
            let cc: [&str; 0] = [];
            let document = activity_id(config, id);
            let context = EmojiReactContext {
                config,
                id: &document,
                actor_ap_id: actor,
                object_uri: object,
                to: &to,
                cc: &cc,
                emoji,
                emoji_url: resolved.emoji_url.as_deref(),
                published,
            };
            Ok(emoji_react_json(&context))
        }
        LocalActivityKind::Follow => Ok(follow_json(
            config,
            &activity_id(config, id),
            actor,
            object,
            published,
        )),
        LocalActivityKind::Block => Ok(block_json(
            config,
            &activity_id(config, id),
            actor,
            object,
            published,
        )),
        LocalActivityKind::Create | LocalActivityKind::Announce => Err(AppError::RouteNotFound),
    }
}

/// The `/users/{nickname}/statuses/{uuid}/activity` core:
/// [`object_core`](super::objects::object_core)'s
/// checks in [`object_core`](super::objects::object_core)'s order — unparsable uuid, non-`ActivityPub`
/// client, `AUTHORIZED_FETCH` gate, then a row this instance does not own,
/// a reblog row, or a nickname that is not the live local author — with
/// one difference: a soft-deleted status is the 404 JSON here, not a
/// `Tombstone`. The survivor's [`note_json`] document rides out wrapped in
/// [`create_activity_json`].
async fn notice_activity_core(
    state: &AppState,
    id_text: &str,
    nickname: &str,
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
    let Some(stored) = status::get_by_id_including_deleted(&state.db, id).await? else {
        return Err(AppError::RouteNotFound);
    };
    if !stored.local || stored.reblog_of_id.is_some() || stored.deleted_at.is_some() {
        return Err(AppError::RouteNotFound);
    }
    if !nickname_owns(state, Some(nickname), &stored).await? {
        return Err(AppError::RouteNotFound);
    }
    if !read_admitted(state, &stored, headers, uri).await? {
        return Err(AppError::RouteNotFound);
    }
    Ok(document_response(&create_document(state, &stored).await?))
}
