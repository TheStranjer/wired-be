//! The status endpoints: reads, favourite writes, the favourites listings,
//! translation and the delete, all over one shared status gate and page
//! renderer (`docs/ARCHITECTURE.md` decision 1).
//!
//! The routes are the lookups `GET /api/v1/statuses/{id}` and the batch
//! `GET /api/v1/statuses?id[]=a&id[]=b`, the favourite writes
//! `POST /api/v1/statuses/{id}/favourite` and its `…/unfavourite` twin,
//! the state listing `GET /api/v1/statuses/{id}/favourited_by`, the
//! caller's own favourites list `GET /api/v1/favourites`,
//! `POST /api/v1/statuses/{id}/translate` and the write
//! `DELETE /api/v1/statuses/{id}`.
//!
//! The reference is Pleroma's `StatusController` (`lib/pleroma/web/
//! mastodon_api/controllers/status_controller.ex`): `show` and `.index`
//! for the reads, `favourite` / `unfavourite` / `favourited_by` /
//! `favourites` for the likes, and `delete` — with
//! `Pleroma.Web.CommonAPI.delete/2`
//! (`lib/pleroma/web/common_api.ex`) behind it — for the write. Those
//! reduce to the same two moves per surface: find the status, ask
//! `Visibility.visible_for_user?`, and either render it or answer
//! `{:error, :not_found}`. Here finding and asking live in the shared
//! [`status_for_viewer_or_404`], which every
//! `/api/v1/statuses/{id}/...` route reuses, and rendering lives in the
//! batched page renderer [`crate::services::render_status`].
//!
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
//! # The favourite writes
//!
//! `POST …/favourite` and `POST …/unfavourite` are Pleroma's `favourite`
//! and `unfavourite`: a user token ([`AuthUser`]) with `write:favourites`,
//! both routes riding the `statuses_actions` throttle bucket like the
//! reports route, and both answering the [`Status`] entity as the caller
//! sees it — `favourited` true after a like, false after a withdrawal, and
//! the stepped `favourites_count`, since the row is re-read for the render.
//! Both writes ([`favourite::favourite`] / [`favourite::unfavourite`]) are
//! idempotent on `(account, status)`: a repeat favourite counts once, and
//! unfavouriting a status the caller never liked is the unchanged entity
//! with 200. Favouriting a status the caller may not see is the shared
//! 404, and a reblog's id favourites the *original* (Mastodon's behaviour,
//! which is where Pleroma's `Like` lands too) while the answer stays the
//! reblog entity.
//!
//! # The favourites listings
//!
//! `GET …/favourited_by` is Pleroma's `favourited_by`: optional auth
//! (again `read:statuses` on a presented user token), the same
//! [`status_for_viewer_or_404`] gate on the status, and the answer an
//! array of [`Account`] entities — the accounts that favourited, newest
//! like first — paged by [`PageParams`] keyed on the *favourite row id*
//! (the `Link` cursors carry like ids, not account ids), 20 by default
//! and 80 at most. The author's
//! [`hide_favorites`](crate::models::user::User::hide_favorites) flag
//! hides *their own* favourites list (the Mastodon rule); it never gates
//! this endpoint.
//!
//! `GET /api/v1/favourites` is the caller's own list: [`AuthUser`] with
//! `read:favourites`, the statuses they favourited newest-favourite
//! first, [`PageParams`] again keyed on the favourite row id, and only
//! the statuses still visible to them — the shared [`filter_visible`]
//! step drops what a later block, a visibility change or a delete took
//! out of their view (Pleroma folds the same narrowing into
//! `ActivityPub.fetch_favourites`). A short page ends the walk, and the
//! `Link` header speaks only of the entries that came back.
//!
//! Neither write federates anything yet: the federation ticket that sends
//! the `Like` activity and its `Undo` wires into this module's
//! `after_favourite_changed` hook, the documented empty landing point,
//! exactly as the reactions' [`crate::api::emoji_reactions`] hook waits
//! for its `EmojiReact`.
//!
//! # The translation
//!
//! `POST /api/v1/statuses/{id}/translate` is Pleroma's `translate`: a user
//! token with `read:statuses`, the same [`status_for_viewer_or_404`] gate (so
//! a post the caller may not read is the shared 404, where Pleroma answers its
//! visibility arm 404 too), and the body's `lang` as the target language —
//! falling back to the caller's own `language` and then to `en`. Which
//! provider that asks, and what its call looks like on the wire, is
//! [`crate::services::translate`]'s business; this route adds only the gate,
//! the answer's [`Translation`] entity and the two 503s an instance without a
//! working provider owes: `"Translation is not enabled"` while none is
//! configured, `"Translation service unavailable"` when the configured one
//! fails.
//!
//! # The delete
//!
//! `DELETE /api/v1/statuses/{id}` is Pleroma's `delete`: a user token
//! ([`AuthUser`]) with `write:statuses`, on the same `statuses_actions`
//! bucket the favourite writes ride, and the same
//! [`status_for_viewer_or_404`] gate in front of the row. Then Pleroma's
//! ownership rule: only the author may delete their own post, while an
//! admin or a moderator ([`is_admin`](crate::models::user::User::is_admin)
//! or [`is_moderator`](crate::models::user::User::is_moderator) — its
//! `messages_delete` privilege) may delete anybody's. The two refusals keep
//! Pleroma's two bodies: the shared 404 for a status that is malformed,
//! unknown, deleted or hidden from this caller, and a 403 that carries the
//! same `"Record not found"` wording for one that is visible but somebody
//! else's.
//!
//! The answer is the [`Status`] entity *as it was before the delete*, with
//! the status's raw source text beside it under the top-level `text` key —
//! Mastodon's "delete and redraw" payload, which is what Pleroma's
//! `with_source: true` renders here. What the write leaves is the tombstone
//! of that status and of every live reblog of it, all counters stepped down
//! in one transaction ([`soft_delete_with_reblogs`]); the media attachment
//! rows of the deleted statuses stay in the table, referenced by nothing
//! live, until the cleanup job removes them.
//!
//! Nothing leaves this instance yet: the `Delete` activity, the
//! `status_deleted` stream push and the notification cleanup all attach at
//! this route's `after_status_deleted` hook, the documented empty landing
//! point.
//!
//! What the reads owe no 422: this module answers an unusable id with the
//! show route's 404 and the batch route's silent skip, and the only
//! request shapes beyond those are the pagination cursors' 400.
//!
//! [`status_for_viewer_or_404`]: crate::api::helpers::status_for_viewer_or_404
//! [`soft_delete_with_reblogs`]: crate::models::status::soft_delete_with_reblogs
//! [`filter_visible`]: crate::models::status_visibility::filter_visible
//! [`Status`]: crate::entities::status::Status
//! [`Account`]: crate::entities::account::Account
//! [`Translation`]: crate::entities::translation::Translation

use std::collections::{HashMap, HashSet};

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, Uri},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
    api::helpers::{requested_status_ids, status_for_viewer_or_404},
    auth::{AuthUser, MaybeAuthUser},
    entities::{
        account::{Account, AccountViewer},
        status::Status,
        translation::Translation,
    },
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        favourite::{self, Favourite as FavouriteRow},
        status::{self, Status as StatusRow},
        status_visibility,
        user::User,
    },
    oauth::scopes::{READ_FAVOURITES, READ_STATUSES, WRITE_FAVOURITES, WRITE_STATUSES},
    pagination::{PageParams, link_header, set_link_header},
    services::{
        render_status::{render_status, render_statuses},
        translate::Translator,
    },
    state::AppState,
};

/// How many ids one batch lookup acts on: Pleroma's `index` `limit`, with
/// every id past it ignored rather than refused.
pub const MAX_STATUS_IDS: usize = 100;

/// The 503 body of `POST …/translate` on an instance with no translation
/// provider configured (Pleroma's "Translation service not configured", in
/// the ticket's wording).
const NOT_ENABLED: &str = "Translation is not enabled";
/// The 503 body of `POST …/translate` when a configured provider did not
/// answer, or answered an error: the reason goes to the log, never here.
const UNAVAILABLE: &str = "Translation service unavailable";
/// The language `POST …/translate` translates into when the body names none
/// and the caller's account has no default of its own either.
const DEFAULT_TARGET_LANGUAGE: &str = "en";

/// The body of both of the delete route's refusals: Pleroma's
/// `StatusController.delete` answers `{:error, :not_found}` for an activity it
/// cannot find, and `CommonAPI.delete`'s ownership arm — the one that
/// [`AppError::Forbidden`] status carries here — renders the same
/// `"Record not found"` wording, so a probe cannot tell "not yours" from
/// "not there".
const RECORD_NOT_FOUND: &str = "Record not found";

/// Page size cap of `favourited_by`: the ticket pins 80, above
/// [`crate::pagination::MAX_LIMIT`], with Pleroma's global 20 as the
/// default the plain [`PageParams::from_query`] keeps.
const FAVOURITED_BY_MAX_LIMIT: i64 = 80;

/// The `Link` headers' query parameters carry nothing but the cursor:
/// neither favourites listing has a filter to repeat on either link (the
/// relationship listings' constant, same rule).
const NO_EXTRA_QUERY: [(String, String); 0] = [];

/// The `/api/v1` statuses router, merged into [`crate::api::v1`].
///
/// `state` reaches the three status writes — the two favourite ones and
/// `DELETE /api/v1/statuses/{id}` — which mount the `statuses_actions`
/// throttle bucket the way the reports route does (see [`crate::rate_limit`]).
pub fn router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/favourites", get(list_favourites))
        .route("/statuses/{id}", get(show_status))
        .route(
            "/statuses/{id}",
            delete(delete_status).layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
        )
        .route("/statuses", get(list_statuses))
        .route(
            "/statuses/{id}/favourite",
            post(favourite_status).layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
        )
        .route(
            "/statuses/{id}/unfavourite",
            post(unfavourite_status)
                .layer(crate::rate_limit::rate_limit(state, "statuses_actions")),
        )
        .route("/statuses/{id}/favourited_by", get(list_favourited_by))
        .route("/statuses/{id}/translate", post(translate_status))
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

/// `DELETE /api/v1/statuses/{id}` — delete the status `id` and answer the
/// [`Status`] entity *as it was before the deletion*, with the status's raw
/// source text beside it under the top-level `text` key (Mastodon's "delete
/// and redraw").
///
/// The gate is the shared [`status_for_viewer_or_404`], so a malformed,
/// unknown, already deleted or invisible id is this module's one 404 —
/// Pleroma's `delete` finds no activity and answers `{:error, :not_found}`.
/// Behind it stands Pleroma's ownership rule, [`may_delete`]: one's own post,
/// or anything at all for staff. Somebody else's visible status is the same
/// `"Record not found"` body under a 403.
///
/// The entity renders from the row the gate read, before
/// [`soft_delete_with_reblogs`] turns that row and every live reblog of it
/// into a tombstone; the whole write is one transaction, so a caller that
/// gets a 200 never meets a half-deleted post. A status that another delete
/// took between the gate and the write answers the 404, as does the one row
/// the renderer cannot answer.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:statuses` check; [`AppError::NotFound`] for a malformed,
/// unknown, deleted or invisible status, for one a concurrent delete reached
/// first, and for the row the render cannot answer;
/// [`AppError::Forbidden`] for a status that is visible but not the caller's;
/// the bucket's 429 before this handler runs. Any database failure arrives
/// through `?` as [`AppError::Database`].
async fn delete_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<DeletedStatus>> {
    auth.require(WRITE_STATUSES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    if !may_delete(&auth.0, &row) {
        return Err(AppError::Forbidden(RECORD_NOT_FOUND.to_owned()));
    }
    let entity = render_status(&state, Some(&auth.0), &row)
        .await?
        .ok_or(AppError::NotFound)?;
    if status::soft_delete_with_reblogs(&state.db, row.id)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound);
    }
    debug!(
        status = %row.id,
        user = %auth.0.id,
        staff = auth.0.is_admin.get() || auth.0.is_moderator.get(),
        "status deleted"
    );
    after_status_deleted(&state, &row);
    Ok(Json(DeletedStatus {
        entity,
        text: row.raw_text,
    }))
}

/// Whether `caller` may delete `row`: its author may, and an admin or a
/// moderator may delete anything — Pleroma's
/// `User.privileged?(user, :messages_delete) || user.ap_id == object.data["actor"]`,
/// whose `messages_delete` privilege the two role flags stand in for here.
/// The rest of the gate (does the row exist, is it live, may this caller see
/// it) is [`status_for_viewer_or_404`]'s, which is why a refusal is a 403 on
/// top of the shared 404 rather than one indistinguishable answer.
fn may_delete(caller: &User, row: &StatusRow) -> bool {
    caller.id == row.user_id || caller.is_admin.get() || caller.is_moderator.get()
}

/// The answer of `DELETE /api/v1/statuses/{id}`: the [`Status`] entity as the
/// caller saw it before the row became a tombstone, with the raw source text
/// beside it under the top-level `text` key. That key is Mastodon's
/// delete-and-redraw payload — Pleroma renders the same one from the object's
/// `source` for this action (`with_source: true`) and never on a plain read,
/// which is why it is this response's own shape rather than a field of the
/// shared entity.
#[derive(Debug, Serialize)]
struct DeletedStatus {
    /// The pre-delete entity, its keys at the top level of the response.
    #[serde(flatten)]
    entity: Status,
    /// The status's stored source text ([`raw_text`](StatusRow::raw_text)) as
    /// it was before the delete; empty for a reblog, which carries none.
    text: String,
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

/// `POST /api/v1/statuses/{id}/favourite` — favourite the status `id` and
/// answer it as the caller sees it: `favourited` true and
/// `favourites_count` stepped, because the row is re-read for the render
/// and the renderer's batched favourites read now names the caller.
///
/// The status resolves through [`status_for_viewer_or_404`] — Pleroma's
/// `CommonAPI.favorite` checks the post is visible to the actor before it
/// builds the `Like`, and what the caller may not see is the same 404 as
/// what does not exist. A reblog row favourites its *original*: Mastodon's
/// behaviour for liking a boost, and the row's id — not the original's —
/// is what the entity answers with. The write is idempotent, so a repeat
/// favourite is one row, one count and one 200.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:favourites` check; [`AppError::NotFound`] for a
/// malformed, unknown, deleted or invisible status, for the row a delete
/// took between the gate and the re-read, and for the one row the render
/// cannot answer; the bucket's 429 before the handler runs. Any database
/// failure arrives through `?` as [`AppError::Database`].
async fn favourite_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_FAVOURITES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    let liked_id = liked_status_id(&row);
    if let Some(created) =
        favourite::favourite(&state.db, &state.config.base_url, auth.0.id, liked_id).await?
    {
        debug!(
            status = %liked_id,
            user = %auth.0.id,
            "status favourited"
        );
        after_favourite_changed(&state, &created, true);
    }
    current_entity(&state, &auth.0, row.id).await
}

/// `POST /api/v1/statuses/{id}/unfavourite` — withdraw the caller's
/// favourite of the status `id` and answer it as they see it, `favourited`
/// false and `favourites_count` stepped back.
///
/// The same gate and the same reblog mapping as [`favourite_status`], and
/// [`favourite::unfavourite`] is content with nothing to delete: a
/// withdrawal of a favourite that was never made (or already withdrawn)
/// changes nothing and is the unchanged [`Status`] with 200, where
/// Pleroma's `unfavorite` answers its error arm.
///
/// # Errors
///
/// Those of [`favourite_status`]. Any database failure arrives through
/// `?` as [`AppError::Database`].
async fn unfavourite_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_FAVOURITES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    let liked_id = liked_status_id(&row);
    if let Some(removed) = favourite::unfavourite(&state.db, auth.0.id, liked_id).await? {
        debug!(
            status = %liked_id,
            user = %auth.0.id,
            "favourite withdrawn"
        );
        after_favourite_changed(&state, &removed, false);
    }
    current_entity(&state, &auth.0, row.id).await
}

/// `GET /api/v1/statuses/{id}/favourited_by` — the [`Account`] entities
/// that favourited the status `id`, newest like first, one [`PageParams`]
/// page at a time (20 by default, at most 80) with the `Link` header over
/// the *favourite row ids*.
///
/// Auth is optional: a presented user token must grant `read:statuses`
/// and decides whose view the accounts render for, an anonymous request
/// sees the same list. The status resolves through
/// [`status_for_viewer_or_404`], so an invisible post hides its favouriters
/// behind the shared 404 — and the author's `hide_favorites` does *not*:
/// that flag hides the author's own favourites list, not a list of who
/// favourited a post of theirs (Mastodon's rule, and Pleroma's
/// `favourited_by` never consults it).
///
/// # Errors
///
/// [`AppError::Forbidden`] when a presented user token lacks
/// `read:statuses`; [`AppError::Unauthorized`] for a token that does not
/// resolve; [`AppError::NotFound`] for a status this route may not answer;
/// [`AppError::BadRequest`] from the page parameters for a malformed
/// cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
async fn list_favourited_by(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require_user_scope(READ_STATUSES)?;
    let page = PageParams::from_query(uri.query(), FAVOURITED_BY_MAX_LIMIT)?;
    let row = status_for_viewer_or_404(&state, auth.0.as_ref(), &id).await?;
    let entries = favourite::users_for_status(&state.db, row.id, page).await?;
    debug!(
        status = %row.id,
        favouriters = entries.len(),
        "status favouriters listed"
    );
    let ids: Vec<Uuid> = entries.iter().map(|entry| entry.favourite_id).collect();
    let mut headers = HeaderMap::new();
    set_link_header(
        &mut headers,
        page_link(&state.config.base_url, uri.path(), &ids).as_deref(),
    )?;
    // The list names no relationship of the viewer to the favouriters;
    // Pleroma's `favourited_by` renders plain `show.json` accounts.
    let viewer: AccountViewer<'_> = auth.0.as_ref();
    let accounts = entries
        .iter()
        .map(|entry| Account::from_user_for(viewer, &entry.user, &state.config, None))
        .collect();
    Ok((headers, Json(accounts)))
}

/// `GET /api/v1/favourites` — the statuses the caller favourited, newest
/// favourite first, one [`PageParams`] page at a time with the `Link`
/// header over the *favourite row ids*, and narrowed to what their eye
/// still allows: [`filter_visible`] drops the posts a later block or a
/// visibility change took out of their view, and the renderer answers the
/// rest exactly as any other page.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user
/// token; [`AppError::Forbidden`] when the token does not grant
/// `read:favourites`; [`AppError::BadRequest`] from the page parameters
/// for a malformed cursor. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
async fn list_favourites(
    State(state): State<AppState>,
    auth: AuthUser,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Status>>)> {
    auth.require(READ_FAVOURITES)?;
    let pairs = favourite::page_for_user(&state.db, auth.0.id, page).await?;
    let rows: Vec<StatusRow> = pairs.iter().map(|(_, row)| row.clone()).collect();
    let visible = status_visibility::filter_visible(&state.db, rows, Some(&auth.0)).await?;
    let visible_ids: HashSet<Uuid> = visible.iter().map(|row| row.id).collect();
    // The cursors key on the like, so they survive the visibility step:
    // only favourites whose status came back visible name the page edges.
    let cursors: Vec<Uuid> = pairs
        .iter()
        .filter(|(favourite, _)| visible_ids.contains(&favourite.status_id))
        .map(|(favourite, _)| favourite.id)
        .collect();
    debug!(
        user = %auth.0.id,
        favourites = visible.len(),
        "favourites listed"
    );
    let mut headers = HeaderMap::new();
    set_link_header(
        &mut headers,
        page_link(&state.config.base_url, uri.path(), &cursors).as_deref(),
    )?;
    Ok((
        headers,
        Json(render_statuses(&state, Some(&auth.0), &visible).await?),
    ))
}

/// `POST /api/v1/statuses/{id}/translate` — translate the status `id` into
/// the body's `lang` and answer the [`Translation`] entity.
///
/// The status resolves through [`status_for_viewer_or_404`] like every other
/// route keyed by id: Pleroma's `translate` refuses anything but a `public`
/// or `unlisted` post, and here the visibility model's answer is the shared
/// 404, so a private post translation cannot see is also a post that
/// translation cannot say exists. What is translated is the status's HTML
/// `content`, in the language the provider's own protocol offers for it, and
/// the answer's `detected_source_language` is what the provider says the
/// original was written in.
///
/// Without a configured provider the call cannot succeed: 503
/// `{"error":"Translation is not enabled"}`. A provider that is configured
/// but does not answer — transport failure, timeout, or a 4xx/5xx of its own —
/// is 503 `{"error":"Translation service unavailable"}`, with the real reason
/// in the log and never in the body.
///
/// [`Translation`]: crate::entities::translation::Translation
async fn translate_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<TranslateParams>,
) -> AppResult<Json<Translation>> {
    auth.require(READ_STATUSES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    let translator = Translator::from_config(&state.config, &state.http_client);
    if translator.provider_name().is_none() {
        return Err(AppError::ServiceUnavailable(NOT_ENABLED.to_owned()));
    }
    let target = target_language(params.lang.as_deref(), auth.0.language.as_deref());
    let translated = translator
        .translate(&row.content, row.language.as_deref(), &target)
        .await
        .map_err(|err| {
            warn!(
                %err,
                status = %row.id,
                target = %target,
                "status translation failed"
            );
            AppError::ServiceUnavailable(UNAVAILABLE.to_owned())
        })?;
    debug!(
        status = %row.id,
        provider = translated.provider,
        target = %target,
        "status translated"
    );
    Ok(Json(Translation::for_status(&row, translated)))
}

/// The body of `POST /api/v1/statuses/{id}/translate`.
#[derive(Debug, Deserialize)]
struct TranslateParams {
    /// The language to translate into; absent (or blank) asks for the
    /// caller's own default — see [`target_language`].
    #[serde(default)]
    lang: Option<String>,
}

/// The language `POST …/translate` moves a status into: the body's `lang`,
/// else the caller's account default — the `users.language` column the
/// `Status` entity shows as `source.language` — else
/// [`DEFAULT_TARGET_LANGUAGE`]. That is Pleroma's
/// `Map.get(params, :lang) || user.language` with a fallback for the account
/// that never named a language, since a blank target is no language to ask a
/// provider for; a blank spelling of either is no language either.
fn target_language(requested: Option<&str>, preferred: Option<&str>) -> String {
    let asked = requested.map(str::trim).filter(|lang| !lang.is_empty());
    let default = preferred.map(str::trim).filter(|lang| !lang.is_empty());
    asked
        .or(default)
        .unwrap_or(DEFAULT_TARGET_LANGUAGE)
        .to_owned()
}

/// The one status a write answers: the row `viewer` just changed —
/// a reblog row still answers *itself* — re-read first so the render
/// carries the counter the write stepped, then rendered for them.
///
/// # Errors
///
/// [`AppError::NotFound`] when the row is gone — deleted between the
/// visibility gate and this re-read — and for the one row the renderer
/// answers `None` for (a reblog whose original went away in the same
/// window; [`crate::api::emoji_reactions`] answers it the same way); any
/// database failure arrives through `?` as [`AppError::Database`].
async fn current_entity(state: &AppState, viewer: &User, id: Uuid) -> AppResult<Json<Status>> {
    let row = status::get_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let entity = render_status(state, Some(viewer), &row)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(entity))
}

/// The `Link` header of a favourites page: [`link_header`] over the first
/// (newest) and last (oldest) favourite id of the page, the follow lists'
/// [`page_link`](crate::api::accounts) shape with the like id in place of
/// the relationship id's. An empty page names no cursor and links nowhere.
fn page_link(base_url: &str, path: &str, favourite_ids: &[Uuid]) -> Option<String> {
    let newest = favourite_ids.first()?.to_string();
    let oldest = favourite_ids.last()?.to_string();
    link_header(base_url, path, &NO_EXTRA_QUERY, &newest, &oldest)
}

/// The status a favourite write lands on: `row` itself for a plain post,
/// and its **original** for a reblog row — Mastodon's behaviour for liking
/// a boost, mirrored here (and in the renderer's flag lookup, where the
/// same rule keys `favourited`). The endpoint still answers the row the
/// client named.
const fn liked_status_id(row: &status::Status) -> Uuid {
    match row.reblog_of_id {
        Some(original) => original,
        None => row.id,
    }
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

/// Post-write hook of both favourite routes: the federation ticket that
/// sends the `Like` activity (and its `Undo`, when a favourite is
/// withdrawn) wires the delivery in here — the activity to send is
/// [`FavouriteRow::ap_id`], the pair it names is [`FavouriteRow::user_id`]
/// and [`FavouriteRow::status_id`], and the queueing happens through
/// `state` (decision 11). The notification ticket that tells the status's
/// author lands in the same hook; Pleroma runs both through its
/// `CommonAPI.favorite` pipeline.
///
/// `favouriting` states which way the write moved the fact: `true` for the
/// favourite a `POST …/favourite` left behind — a repeat favourite never
/// reaches this hook, since it changed nothing — and `false` for the
/// `Undo` of the favourite a `POST …/unfavourite` removed. A withdrawal
/// that found no row to remove never reaches it either.
///
/// Intentionally empty until then, like the reactions'
/// [`after_reaction_changed`](crate::api::emoji_reactions::after_reaction_changed)
/// and the blocks'
/// [`after_block_changed`](crate::api::blocks::after_block_changed).
const fn after_favourite_changed(_state: &AppState, _favourite: &FavouriteRow, _favouriting: bool) {
}

/// Post-write hook of the delete route: the federation ticket that sends the
/// `Delete` activity (and, with it, the withdrawal of every reblog this delete
/// took down), the streaming ticket that pushes `status_deleted` to the
/// timelines that carried the post, and the notification ticket that clears the
/// notifications it caused all wire in here, and the queueing happens through
/// `state` (decision 11).
///
/// `status` is the row **as it was before the delete** — the delete clears the
/// stored text, so this is the only copy of what the post said: its
/// [`id`](StatusRow::id) and [`uri`](StatusRow::uri) name the object to
/// withdraw, [`user_id`](StatusRow::user_id) is the actor that owns it, and
/// [`reblog_of_id`](StatusRow::reblog_of_id) says whether the object was a
/// boost whose original must stop counting it. Pleroma runs all three off the
/// same `CommonAPI.delete` pipeline.
///
/// Intentionally empty until then, like [`after_favourite_changed`] above and
/// the reactions'
/// [`after_reaction_changed`](crate::api::emoji_reactions::after_reaction_changed).
const fn after_status_deleted(_state: &AppState, _status: &StatusRow) {}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use serde_json::{Value, json};
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

    #[test]
    fn the_link_pages_off_the_first_and_last_like_ids() {
        let (newest, middle, oldest) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let link = page_link(
            "https://host",
            "/api/v1/favourites",
            &[newest, middle, oldest],
        )
        .expect("a page with ids links");
        assert_eq!(
            link,
            format!(
                "<https://host/api/v1/favourites?max_id={oldest}>; rel=\"next\", \
                 <https://host/api/v1/favourites?min_id={newest}>; rel=\"prev\""
            ),
            "next walks to the oldest like, prev back to the newest, and \
             the middle id names nothing"
        );
    }

    #[test]
    fn an_empty_page_links_nowhere() {
        assert_eq!(
            page_link("https://host", "/api/v1/favourites", &[]),
            None,
            "no rows, no cursors, no Link header"
        );
    }

    #[test]
    fn a_reblog_like_is_keyed_to_its_target() {
        // The mapping both writes hand the model: the reblog row's id is
        // what the client named, the original's is where the like lives.
        let (reblog, original) = (Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(
            liked_status_id(&row(reblog)),
            reblog,
            "a plain post likes itself"
        );
        let mut boost = row(reblog);
        boost.reblog_of_id = Some(original);
        assert_eq!(
            liked_status_id(&boost),
            original,
            "a boost is liked through its original"
        );
    }

    /// The body of a translate request, as `ApiBody` would hand it over:
    /// parsed from the JSON shape the extractor builds for either accepted
    /// content type.
    fn params(body: Value) -> TranslateParams {
        serde_json::from_value(body).expect("the extractor's shape parses")
    }

    #[test]
    fn the_body_carries_one_optional_target_language() {
        assert_eq!(
            params(json!({"lang": "de"})).lang.as_deref(),
            Some("de"),
            "the documented key"
        );
        assert_eq!(
            params(json!({})).lang,
            None,
            "an empty body — which is also what an absent body and a bare              `lang=` form parameter stand for — names no language"
        );
    }

    #[test]
    fn the_asked_language_beats_the_account_default() {
        assert_eq!(
            target_language(Some("eo"), Some("de")),
            "eo",
            "the body's `lang` wins"
        );
        assert_eq!(
            target_language(None, Some("de")),
            "de",
            "without it the caller's own default"
        );
        assert_eq!(
            target_language(Some(" fr "), Some("de")),
            "fr",
            "and a padded spelling is trimmed, not rejected"
        );
    }

    #[test]
    fn english_is_the_last_resort() {
        for (asked, preferred) in [
            (None, None),
            (Some(""), None),
            (Some("   "), Some("")),
            (None, Some("  ")),
        ] {
            assert_eq!(
                target_language(asked, preferred),
                "en",
                "a blank is no language to ask a provider for, so `{asked:?}`                  with `{preferred:?}` falls back to English"
            );
        }
    }
}
