//! The inbox routes of the `ActivityPub` router: the three shared `POST`s
//! (`/inbox`, `/internal/fetch/inbox`, `/relay/inbox`) and the personal
//! `POST /users/{nickname}/inbox`, each answering an authenticated
//! delivery with exactly one queued [`KIND_INCOMING`] job, beside the
//! owner's C2S `GET /users/{nickname}/inbox` — Pleroma's `read_inbox`.

use axum::{
    extract::{FromRequest as _, Path, Request, State},
    http::{StatusCode, Uri},
    response::{IntoResponse as _, Response},
};
use serde_json::{Value, json};
use tracing::debug;
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    error::{AppError, AppResult},
    federation::{
        collections,
        dispatch::{KIND_INCOMING, dedupe_key, synthesize_id},
        inbox_auth::{InboxAuthError, SignedActivity},
        json::first_type,
        note::{create_activity_json, load_note_context, note_json},
    },
    jobs::QUEUE_FEDERATOR_INCOMING,
    models::{
        job::{NewJob, enqueue},
        mention, status,
        user::{self, get_by_nickname},
    },
    oauth::scopes::{READ, READ_STATUSES},
    state::AppState,
};

use super::negotiation::document_response;
use super::outbox_create::ensure_own_actor;
use super::{
    FEDERATION_DISABLED, INBOX_PAGE_SIZE, INCOMING_MAX_ATTEMPTS, INCOMING_UNIQUE_PREFIX,
    INVALID_ACTIVITY, ORDERED_COLLECTION,
};

/// Either shared inbox — the instance's `POST /inbox`, the instance
/// actor's `POST /internal/fetch/inbox` and the relay actor's
/// `POST /relay/inbox`, the paths Pleroma's `inbox` action answers on
/// besides the personal one (its `/relay` route is a different service
/// actor; here the same instance actor stands behind both addresses, so
/// the same queueing handler answers all three). A delivery accepted here
/// is queued with a null `inbox_owner`: no local account claims it, the
/// instance does.
pub(super) async fn inbox(State(state): State<AppState>, request: Request) -> Response {
    inbox_answer(&state, None, request)
        .await
        .unwrap_or_else(InboxAuthError::into_response)
}

/// `POST /users/{nickname}/inbox` — the personal inbox of the local account
/// `nickname`: like the shared ones, but the named account must be a live
/// local user or the delivery is the plain 404 (step 1 of the ticket), and
/// the queued job records it as the activity's `inbox_owner`.
pub(super) async fn user_inbox(
    State(state): State<AppState>,
    Path(nickname): Path<String>,
    request: Request,
) -> Response {
    inbox_answer(&state, Some(&nickname), request)
        .await
        .unwrap_or_else(InboxAuthError::into_response)
}

/// Everything an accepted `ActivityPub` delivery meets, in the order the
/// ticket fixes it: the `FEDERATION_ENABLED` gate first — a non-federating
/// instance refuses the door without authenticating, fetching or queueing
/// anything — then the [`SignedActivity`] extractor's authentication (whose
/// 400/401/403/413/415 answers, and the accepted-and-dropped `Delete` of a
/// gone actor, live in [`crate::federation::inbox_auth`]), then the owner
/// check of a personal inbox (`Some(nickname)`), then the activity's
/// sanity, then exactly one [`KIND_INCOMING`] job and the 202. Nothing is
/// ever processed here: the worker runs it.
async fn inbox_answer(
    state: &AppState,
    owner_nickname: Option<&str>,
    request: Request,
) -> Result<Response, InboxAuthError> {
    if !state.config.federation_enabled.get() {
        debug!("inbox delivery refused: federation is disabled");
        return Err(AppError::Forbidden(FEDERATION_DISABLED.to_owned()).into());
    }
    let mut activity = SignedActivity::from_request(request, state).await?;
    let owner_id = inbox_owner_id(state, owner_nickname).await?;
    give_idless_activity_an_id(&mut activity);
    let activity_id = require_activity_fields(&activity.json)?;
    enqueue_incoming(state, &activity, owner_id, activity_id).await?;
    Ok(StatusCode::ACCEPTED.into_response())
}

/// The `users.id` to record as the queued job's `inbox_owner`: the live
/// local account behind a personal inbox route, or null for the shared
/// inboxes, which no account owns. A nickname that is unknown, a remote
/// mirror's, or a deactivated account's is the plain 404
/// `{"error":"Not found"}` — the route keeps whether anyone by that name
/// used to exist to itself.
async fn inbox_owner_id(state: &AppState, nickname: Option<&str>) -> AppResult<Option<Uuid>> {
    let Some(name) = nickname else {
        return Ok(None);
    };
    let owner = get_by_nickname(&state.db, name)
        .await?
        .filter(user::User::is_local_active)
        .ok_or(AppError::RouteNotFound)?;
    Ok(Some(owner.id))
}

/// Gives an `Add` or `Remove` that arrived without an `id` one made up from
/// its body, so it dedupes and routes like any other activity.
///
/// Mastodon sends pins, unpins and featured hashtags this way (its
/// `add_note_serializer.rb` and kin emit only `type`, `actor`, `target` and
/// `object`). The id is the signer's actor id plus a digest of the canonical
/// body ([`synthesize_id`]); every other id-less activity keeps failing
/// [`require_activity_fields`], and an `Add` or `Remove` without a `type`
/// or `actor` still does.
fn give_idless_activity_an_id(activity: &mut SignedActivity) {
    let idless = activity.json.get("id").is_none_or(Value::is_null);
    let pin_like = activity
        .json
        .get("type")
        .and_then(first_type)
        .is_some_and(|kind| kind == "Add" || kind == "Remove");
    if !idless || !pin_like || activity.json.get("actor").is_none() {
        return;
    }
    let id = synthesize_id(&activity.json, &activity.signer.ap_id);
    if let Some(object) = activity.json.as_object_mut() {
        let _previous = object.insert("id".to_owned(), Value::String(id));
    }
}

/// The quick sanity every authenticated activity must pass: a string
/// `id`, a string `type` and an `actor`, or the ticket's 400. The
/// [`SignedActivity`] extractor has already made `actor` a fact (it must
/// name the signer), and the one id-less document the fediverse accepts —
/// Mastodon's `Delete` of an actor this instance never saw — was answered
/// `202` and dropped inside the extractor, so anything failing here
/// describes no activity the dispatcher could fold. Returns the `id` for
/// the deduplication key.
fn require_activity_fields(json: &Value) -> AppResult<&str> {
    let invalid = || AppError::BadRequest(INVALID_ACTIVITY.to_owned());
    let id = json.get("id").and_then(Value::as_str).ok_or_else(invalid)?;
    if !json.get("type").is_some_and(Value::is_string) {
        return Err(invalid());
    }
    if json.get("actor").is_none() {
        return Err(invalid());
    }
    Ok(id)
}

/// Queues the authenticated `activity` for processing: one
/// [`KIND_INCOMING`] job on [`QUEUE_FEDERATOR_INCOMING`] with the
/// [`INCOMING_MAX_ATTEMPTS`] budget, carrying the activity's JSON, the
/// signer's id and the local owner's id (null for the shared inboxes),
/// deduplicated on `incoming:{dedupe key}` (the activity id, plus an object
/// digest for an `Update`, see [`dedupe_key`]) so a redelivered activity is
/// processed once. A duplicate of a still-live job is swallowed by the
/// queue and changes the answer not at all — both deliveries are
/// accepted, one job exists.
async fn enqueue_incoming(
    state: &AppState,
    activity: &SignedActivity,
    owner_id: Option<Uuid>,
    activity_id: &str,
) -> AppResult<()> {
    let identity = dedupe_key(&activity.json).unwrap_or_else(|| activity_id.to_owned());
    let unique_key = format!("{INCOMING_UNIQUE_PREFIX}:{identity}");
    let queued = enqueue(
        &state.db,
        NewJob {
            queue: QUEUE_FEDERATOR_INCOMING,
            kind: KIND_INCOMING,
            args: json!({
                "activity": activity.json,
                "signer_id": activity.signer.id,
                "inbox_owner": owner_id,
            }),
            run_at: None,
            max_attempts: Some(INCOMING_MAX_ATTEMPTS),
            unique_key: Some(&unique_key),
        },
    )
    .await?;
    if let Some(job) = queued {
        debug!(job_id = %job.id, activity_id, "queued incoming activity");
    } else {
        debug!(activity_id, "incoming activity already queued");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Client-to-server: the owner's reads and writes at the same addresses
// (Pleroma's `read_inbox`, `update_outbox`, `whoami` and `upload_media`).
// ---------------------------------------------------------------------------

/// `GET /users/{nickname}/inbox` — the owner's personal inbox, Pleroma's
/// `read_inbox`: the collection root, or with `?page=true` a newest-first
/// page (20 per page, cursor `max_id`) of the `Create` activities of the
/// statuses addressed to the account — the ones that @-mention it
/// (`status_mentions`: the replies, the direct messages and the mentioned
/// posts all link the addressee there), rendered exactly as the outbox
/// renders its own. The gates are the ticket's: a bearer token granting
/// `read` or `read:statuses` (without one the extractor's 401 answers,
/// Pleroma's `EnsureAuthenticatedPlug` word for word), and the token's
/// user must *be* the `{nickname}` of the path — every other pairing is
/// the 403. No `Accept` negotiation and no HTTP Signature: this is the
/// client API on an actor address, not the document a peer fetches.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the scope check; [`AppError::Forbidden`] with
/// [`FORBIDDEN`](super::FORBIDDEN) for
/// somebody else's inbox; [`AppError::BadRequest`] from the page cursors;
/// any database failure arrives through `?` as [`AppError::Database`].
pub(super) async fn read_inbox(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(nickname): Path<String>,
    uri: Uri,
) -> AppResult<Response> {
    auth.require_any(&[READ, READ_STATUSES])?;
    ensure_own_actor(&auth.0, &nickname)?;
    let collection = user::inbox(&auth.0.ap_id);
    let document = if collections::page_requested(uri.query()) {
        inbox_page(&state, auth.0.id, &collection, uri.query()).await?
    } else {
        collections::collection_root(&collection, ORDERED_COLLECTION, None, &state.config)
    };
    Ok(document_response(&document))
}

/// One `?page=true` of the personal inbox: the ids of the statuses
/// mentioning the owner, newest first off the keyset (decision 4), as
/// their `Create` activities inside the same page envelope the outbox
/// writes.
async fn inbox_page(
    state: &AppState,
    owner_id: Uuid,
    collection: &str,
    query: Option<&str>,
) -> AppResult<Value> {
    let page = collections::keyset_page(query, INBOX_PAGE_SIZE)?;
    let ids = mention::mentioned_status_ids(&state.db, owner_id, page).await?;
    let items = inbox_creates(state, &ids).await?;
    let links = collections::descending_page_links(
        collection,
        query,
        ids.first().copied(),
        ids.last().copied(),
        i64::try_from(ids.len()).unwrap_or(i64::MAX) == INBOX_PAGE_SIZE,
    );
    Ok(collections::ordered_collection_page(
        &links,
        items,
        &state.config,
    ))
}

/// The page's status ids as `Create` activities, page order kept: the
/// live rows come back through one batch read and each renders through
/// the outbox's own loader. A row that vanished between the id page and
/// its render — or that federates as nothing (`local` visibility) — drops
/// out rather than failing the page.
async fn inbox_creates(state: &AppState, ids: &[Uuid]) -> AppResult<Vec<Value>> {
    let rows = status::get_many(&state.db, ids).await?;
    let mut items = Vec::with_capacity(rows.len());
    for row in ids
        .iter()
        .filter_map(|id| rows.iter().find(|row| row.id == *id))
    {
        items.extend(create_activity_of(state, row.id).await?);
    }
    Ok(items)
}

/// One live status row as its `Create` activity, `None` when its object
/// is gone or wears no document.
async fn create_activity_of(state: &AppState, status_id: Uuid) -> AppResult<Option<Value>> {
    match load_note_context(&state.db, &state.config, status_id).await {
        Ok(ctx) => {
            Ok(note_json(&ctx)
                .map(|object| create_activity_json(&object, &ctx.status, &state.config)))
        }
        // The row fell between the id page and its render.
        Err(AppError::NotFound) => Ok(None),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An activity must carry a string `id`, a string `type` and an
    /// `actor`; anything else is the ticket's 400, and the id that passes
    /// is the one returned for the deduplication key.
    #[test]
    fn an_activity_needs_a_string_id_a_string_type_and_an_actor() {
        let good = json!({
            "id": "https://peer.example/activities/1",
            "type": "Follow",
            "actor": "https://peer.example/users/alice",
        });
        let id = require_activity_fields(&good).expect("a complete activity passes");
        assert_eq!(
            id, "https://peer.example/activities/1",
            "the route's answer is the activity's own id"
        );

        for missing in ["id", "type", "actor"] {
            let mut broken = good.clone();
            let _removed = broken
                .as_object_mut()
                .expect("an activity is a JSON object")
                .remove(missing);
            let err = require_activity_fields(&broken)
                .expect_err("a missing field makes the activity invalid");
            assert_eq!(
                err.to_string(),
                INVALID_ACTIVITY,
                "without {missing} the delivery is invalid"
            );
        }

        let numeric_id = json!({"id": 7, "type": "Follow", "actor": "a"});
        assert_eq!(
            require_activity_fields(&numeric_id)
                .expect_err("a non-string id is no id")
                .to_string(),
            INVALID_ACTIVITY,
            "numbers do not name activities"
        );
        let array_type = json!({"id": "x", "type": ["Follow"], "actor": "a"});
        assert_eq!(
            require_activity_fields(&array_type)
                .expect_err("an array type is no string type")
                .to_string(),
            INVALID_ACTIVITY,
            "the ticket asks for a string type"
        );
        let object_actor = json!({"id": "x", "type": "Follow", "actor": {"id": "a"}});
        assert!(
            require_activity_fields(&object_actor).is_ok(),
            "an embedded actor object still names an actor"
        );
    }
}
