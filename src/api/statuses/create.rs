//! # The create
//!
//! `POST /api/v1/statuses` is the module's other half, and Pleroma's
//! `create`: a user token granting `write:statuses` (its own scope, not the
//! root) on the `statuses_actions` throttle, the body read through
//! [`ApiParams`] so JSON, form and query parameters arrive the same way,
//! and the whole decision — what the text says, who it mentions, what it
//! tags, how visible it is and what the author's settings default to —
//! delegated to [`crate::services::create_status`]. The handler adds the
//! gate, the client behind the token, the JSON envelope, and the two
//! request-level arms Pleroma hangs off this route: an `Idempotency-Key`
//! header (its own [`IdempotencyPlug`] stands the create's replay on a
//! cache; here the claim is a row in
//! [`crate::models::idempotency_key`], inserted inside the status's own
//! transaction, so a retry of a key that created a live status answers that
//! status and creates nothing) and a `preview` parameter (Pleroma's
//! `draft.preview?`: every check and the full render run, and the entity a
//! create would have answered is built in memory without a write or a
//! hook). A post that passes answers **200** with the [`Status`] entity
//! rendered for its author. What a fresh status still owes the wider
//! federation — and every other post-write telling — hangs off that
//! service's single documented hook, where the mention notifications and
//! the streaming `update` push already run, so this file never grows a
//! second write path. The body may also carry Pleroma's `expires_in` (top-level
//! only, whole seconds): the service turns an accepted span into the row's
//! `expires_at`, and a post wearing one schedules its own delete — a
//! [`statuses.expire`](crate::jobs::statuses::KIND_EXPIRE) job due at that
//! instant, with the periodic
//! [`statuses.expire_sweep`](crate::jobs::statuses::KIND_SWEEP) pass behind
//! it (see [`crate::jobs::statuses`]). And `in_reply_to_id` makes the post
//! an answer to a stored status: the service reads the parent with this
//! route's own [`status_for_viewer_or_404`] rules (so a malformed, unknown,
//! deleted or poster-invisible parent is the same 404 the show route
//! answers, and a reblog row folds to its original) and inherits the
//! thread, author link, capped visibility, fallback language and
//! auto-mentions described in that module. And `media_ids` — decision 10's
//! id-array, JSON's `["a","b"]` or the form's `media_ids[]=a` — posts with
//! attachments: at most [`Config::max_media_attachments`] of them (422
//! `Too many attachments`), each one the poster's own upload no status has
//! claimed (422 `Media not found` for an unknown, foreign or already-used
//! id), claimed for the post on the insert's own transaction at the order
//! the body asked for and answered back in the entity's
//! `media_attachments`. A post that carries at least one attachment may
//! leave its text empty, and attachments ride every visibility, `direct`
//! included. And `in_reply_to_conversation_id` — WBE-123's Pleroma arm —
//! posts into a DM thread by the poster's own
//! [`Participation`](crate::models::conversation::Participation) id: the
//! participation's recipients join its mentions, its conversation becomes
//! the new status's `direct_conversation_id`, and the visibility is forced
//! to `direct` whatever the body asked for; a malformed, unknown or
//! somebody-else's participation id is the same shared 404. And
//! `scheduled_at` — an RFC3339 instant — parks the post instead of writing
//! it: [`crate::services::schedule_status`] runs the very same body checks,
//! then the instance's future window, the account's schedule limits and the
//! media reservation, and stores the normalised request in
//! [`scheduled_statuses`](crate::models::scheduled_status) with one
//! [`scheduled_statuses.publish`](crate::jobs::scheduled_statuses::KIND_PUBLISH)
//! job due at that instant. The answer is **200** with the
//! [`ScheduledStatus`] entity — its `media_attachments` read back from
//! `media_attachments` in the order the body submitted them, though no
//! attachment row is touched — and nothing else happens: no `statuses`
//! row, no `after_status_created` hook, no expiration job. An
//! `Idempotency-Key` on the request is stored on the schedule's row (and
//! echoed into the params and the entity): a retry carrying the same key
//! answers the schedule it first parked rather than parking another. The
//! fire itself claims no key, because the publish replays through
//! [`create_status::create_status`] rather than its replayable twin.
//!
//! [`Config::max_media_attachments`]: crate::config::Config::max_media_attachments
//!
//! [`ApiParams`]: crate::extract::ApiParams
//! [`IdempotencyPlug`]: crate::models::idempotency_key
//! [`Status`]: crate::entities::status::Status
//! [`ScheduledStatus`]: crate::entities::scheduled_status::ScheduledStatus
//! [`status_for_viewer_or_404`]: crate::api::helpers::status_for_viewer_or_404

use std::collections::HashMap;

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, HeaderName},
    response::{IntoResponse as _, Response},
};
use uuid::Uuid;

use crate::{
    auth::AuthUser,
    entities::{media_attachment::MediaAttachment, scheduled_status::ScheduledStatus},
    error::AppResult,
    extract::ApiParams,
    models::{
        media_attachment::{self, MediaAttachment as AttachmentRow},
        oauth_app::{self, OauthApp},
        user::User,
    },
    oauth::scopes::WRITE_STATUSES,
    services::{
        create_status::{self, CreateStatusParams},
        schedule_status,
    },
    state::AppState,
    util::id::parse_id,
};

use super::history::media_entities;

/// `POST /api/v1/statuses` — create the status the body describes and answer
/// its [`Status`] entity as its author sees it, or — when the body names a
/// `scheduled_at` — park the whole create and answer a [`ScheduledStatus`]
/// entity instead.
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
/// Three request-level decisions are this handler's own, all read before the
/// service is asked for anything, in this order. A body naming
/// `scheduled_at` is not a create: [`schedule_status::schedule_status`]
/// checks it with the create's own rules, then the window, the account's
/// schedule limits and the media reservation, stores the normalised request
/// as a pending schedule and queues its fire ([`ScheduledStatus`] answers,
/// with the reserved attachments loaded from their untouched rows in
/// submission order; see [`scheduled_media`]); the request's
/// `Idempotency-Key` is stored on the schedule, so a replayed key answers
/// the schedule it first parked (nothing new is written or queued); the
/// publish claims no key and posts through
/// [`create_status::create_status`] like any other fire. The
/// body's `preview` (a [`flex`] boolean, body or query, default false)
/// answers the entity a create would have
/// returned — validated, rendered, mentions and tags resolved, a fresh
/// minted id, `created_at` now and zero counters — without persisting
/// anything, firing a hook or consulting a key ([`create_status::preview_status`],
/// Pleroma's `draft.preview?` arm). An `Idempotency-Key` header makes the
/// create replayable: a key this account has used for a live status answers
/// that status's entity and creates nothing; otherwise the create runs and
/// claims the key on its own transaction
/// ([`create_status::create_status_idempotent`], this instance's durable
/// stand-in for Pleroma's [`IdempotencyPlug`] cache). A preview ignores the
/// header entirely, and a header longer than [`MAX_IDEMPOTENCY_KEY_CHARS`]
/// characters, repeated, or holding non-visible bytes is no key at all —
/// the plain create.
///
/// [`flex`]: crate::extract::flex
/// [`IdempotencyPlug`]: crate::models::idempotency_key
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:statuses` check; [`AppError::RateLimited`] from the
/// bucket; whatever [`create_status::create_status`] refuses — 422 for an
/// empty text over no attachments, an over-long status, an over-long
/// warning, an unknown `visibility`, an unknown `content_type`, more
/// `media_ids` than the instance's attachment limit, a `media_ids` entry
/// the poster may not claim, or an `expires_in` under the instance's floor
/// or past the calendar — plus the extractor's own 422 for an `expires_in`
/// that is no integer — plus its own database
/// failures as [`AppError::Database`]. An `in_reply_to_id` naming no
/// status the poster may see — malformed, unknown, deleted or invisible —
/// is the shared 404 `{"error":"Record not found"}`, exactly the show
/// route's answer for the same id, and so is an
/// `in_reply_to_conversation_id` naming no participation the poster owns.
/// A preview owes the same 422s: validation runs on it exactly as on a
/// post. A scheduled create owes the same body 422s and the same 404s
/// before its own: 422 `Invalid scheduled_at` for text that is no RFC3339
/// instant, 422 naming the instance's future window for a moment inside
/// it (a past one included), 422 `Too many scheduled statuses` for an
/// account at either schedule limit, and 422 `Media not found` for an
/// attachment it may not reserve.
pub(super) async fn create_status(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    ApiParams(params): ApiParams<CreateStatusParams>,
) -> AppResult<Response> {
    auth.require(WRITE_STATUSES)?;
    let app: Option<OauthApp> = oauth_app::get_by_id(&state.db, auth.1.app_id).await?;
    if params.scheduled_at.is_some() {
        let entity = schedule_create(&state, &auth.0, app.as_ref(), &headers, params).await?;
        return Ok(Json(entity).into_response());
    }
    if params.previews() {
        return Ok(Json(
            create_status::preview_status(&state, &auth.0, app.as_ref(), &params).await?,
        )
        .into_response());
    }
    let entity = match idempotency_key(&headers) {
        Some(key) => {
            create_status::create_status_idempotent(&state, &auth.0, app.as_ref(), params, &key)
                .await?
        }
        None => create_status::create_status(&state, &auth.0, app.as_ref(), params).await?,
    };
    Ok(Json(entity).into_response())
}

/// The `scheduled_at` arm of the create: park the request through
/// [`schedule_status::schedule_status`] — the request's `Idempotency-Key`,
/// if it had one, travels along to be stored on the row and a replay of it
/// answers the schedule already parked — and finish the [`ScheduledStatus`] entity with the
/// schedule's reserved attachments, read by
/// [`media_attachment::get_many`] and answered in the order the body
/// submitted them (the read is id-ordered; this one is not). No media
/// row is written or moved by this route: the reservation is the pending
/// row's params.
///
/// # Errors
///
/// Those of [`schedule_status::schedule_status`] and any database failure
/// behind the media read, as [`AppError::Database`].
async fn schedule_create(
    state: &AppState,
    user: &User,
    app: Option<&OauthApp>,
    headers: &HeaderMap,
    params: CreateStatusParams,
) -> AppResult<ScheduledStatus> {
    let key = idempotency_key(headers);
    let (mut entity, _row) =
        schedule_status::schedule_status(state, user, app, key.as_deref(), params).await?;
    entity.media_attachments = scheduled_media(state, &entity.params.media_ids).await?;
    Ok(entity)
}

/// The `media_attachments` of a schedule answer: the reserved ids the
/// stored params name, in that submitted order, each rendered through
/// the [`MediaAttachment`] entity from its untouched row; an id whose
/// row has since been deleted simply contributes nothing.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn scheduled_media(state: &AppState, texts: &[String]) -> AppResult<Vec<MediaAttachment>> {
    let ids: Vec<Uuid> = texts.iter().filter_map(|text| parse_id(text)).collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = media_attachment::get_many(&state.db, &ids).await?;
    let by_id: HashMap<Uuid, AttachmentRow> =
        rows.into_iter().map(|found| (found.id, found)).collect();
    Ok(media_entities(&ids, &by_id, &state.config))
}

/// The header whose value makes a create replayable (Pleroma's
/// `IdempotencyPlug` key, [`crate::models::idempotency_key`]'s table rows).
const IDEMPOTENCY_KEY_HEADER: HeaderName = HeaderName::from_static("idempotency-key");

/// Longest `Idempotency-Key` this endpoint honors: a longer value is ignored
/// (the create is a plain one and no key is stored), so the `key` column
/// never holds more than this many characters and no client can turn the
/// replay lookup into a large-value index probe.
const MAX_IDEMPOTENCY_KEY_CHARS: usize = 255;

/// The `Idempotency-Key` header, read Pleroma's way (its plug's
/// `get_req_header` single-value arm): exactly one visible-text value of at
/// most [`MAX_IDEMPOTENCY_KEY_CHARS`] characters is the key; an absent
/// header, a repeated one, bytes no header text can hold and an over-long
/// value are all no key at all.
fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    let mut values = headers.get_all(IDEMPOTENCY_KEY_HEADER).iter();
    let key = values.next()?.to_str().ok()?.to_owned();
    values
        .next()
        .is_none()
        .then_some(key)
        .filter(|key| key.chars().count() <= MAX_IDEMPOTENCY_KEY_CHARS)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    /// A header map carrying `values` under the idempotency header.
    fn keyed(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            let _added: bool = headers.append(
                IDEMPOTENCY_KEY_HEADER,
                axum::http::HeaderValue::from_str(value).expect("a visible test value"),
            );
        }
        headers
    }

    #[test]
    fn exactly_one_idempotency_key_header_is_the_key() {
        assert!(idempotency_key(&keyed(&[])).is_none(), "absent is none");
        assert_eq!(
            idempotency_key(&keyed(&["k-1"])).as_deref(),
            Some("k-1"),
            "one value is the key, verbatim"
        );
        assert_eq!(
            idempotency_key(&keyed(&[""])).as_deref(),
            Some(""),
            "the empty value is a key like any other text — Pleroma's plug \
             caches the header's spelling without asking what it means"
        );
        assert!(
            idempotency_key(&keyed(&["a", "b"])).is_none(),
            "a repeated header names no single key and is no key at all"
        );
    }

    #[test]
    fn the_key_cap_is_255_characters_and_longer_is_ignored() {
        let edge = "x".repeat(MAX_IDEMPOTENCY_KEY_CHARS);
        assert_eq!(
            idempotency_key(&keyed(&[&edge])).as_deref(),
            Some(edge.as_str()),
            "the cap itself is a key"
        );
        let over = "x".repeat(MAX_IDEMPOTENCY_KEY_CHARS + 1);
        assert!(
            idempotency_key(&keyed(&[&over])).is_none(),
            "one character past the cap is ignored, not truncated: the \
             create runs as if no header had arrived"
        );
    }

    #[test]
    fn the_preview_flag_is_the_body_s_own_default() {
        let absent = serde_json::from_value::<CreateStatusParams>(json!({"status": "hi"}))
            .expect("the documented shape parses");
        assert!(!absent.previews(), "no key means a real post");
        for value in [json!(true), json!("true"), json!("1"), json!("on")] {
            let body = serde_json::from_value::<CreateStatusParams>(
                json!({"status": "hi", "preview": value}),
            )
            .unwrap_or_else(|_| panic!("`{value}` is a flex-bool spelling"));
            assert!(body.previews(), "`{value}` previews");
        }
        for value in [json!(false), json!("0"), json!("off"), Value::Null] {
            let body = serde_json::from_value::<CreateStatusParams>(
                json!({"status": "hi", "preview": value}),
            )
            .unwrap_or_else(|_| panic!("`{value}` is a flex-bool spelling"));
            assert!(!body.previews(), "`{value}` is as absent as no key");
        }
    }
}
