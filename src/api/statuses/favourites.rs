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
//! A favourite tells the status's author — the `after_favourite_changed`
//! hook lands [`crate::services::notify`]'s favourite telling (and its
//! withdrawal), Pleroma's `Like` notification riding the same pipeline.
//! The same hook federates the write: [`crate::services::federation_out`]
//! sends the `Like` (and its `Undo`) to the status author — and, for a
//! public status, the actor's followers. The reactions'
//! [`crate::api::emoji_reactions`] hook lands the same pair of halves for
//! its `EmojiReact`. The bookmark writes have no such hook to wait
//! for — Pleroma's bookmark is a private row, never an activity.
//!
//! [`Status`]: crate::entities::status::Status
//! [`Account`]: crate::entities::account::Account
//! [`filter_visible`]: crate::models::status_visibility::filter_visible
//! [`status_for_viewer_or_404`]: crate::api::helpers::status_for_viewer_or_404

use std::collections::HashSet;

use axum::{
    Json,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
};
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::status_for_viewer_or_404,
    auth::{AuthUser, MaybeAuthUser},
    entities::{
        account::{Account, AccountViewer},
        status::Status,
    },
    error::AppResult,
    models::{
        favourite::{self, Favourite as FavouriteRow},
        status::Status as StatusRow,
        status_visibility,
    },
    oauth::scopes::{READ_FAVOURITES, READ_STATUSES, WRITE_FAVOURITES},
    pagination::{PageParams, link_for_page, set_link_header},
    services::{
        federation_out, notify,
        render_status::{ViewerMode, render_statuses},
    },
    state::AppState,
};

use super::{FAVOURITED_BY_MAX_LIMIT, action_target_id, current_entity, without_blocked};

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
pub(super) async fn favourite_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_FAVOURITES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    let liked_id = action_target_id(&row);
    if let Some(created) =
        favourite::favourite(&state.db, &state.config.base_url, auth.0.id, liked_id).await?
    {
        debug!(
            status = %liked_id,
            user = %auth.0.id,
            "status favourited"
        );
        after_favourite_changed(&state, &created, true).await;
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
pub(super) async fn unfavourite_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_FAVOURITES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    let liked_id = action_target_id(&row);
    if let Some(removed) = favourite::unfavourite(&state.db, auth.0.id, liked_id).await? {
        debug!(
            status = %liked_id,
            user = %auth.0.id,
            "favourite withdrawn"
        );
        after_favourite_changed(&state, &removed, false).await;
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
/// Deactivated accounts are left out by the query, and the accounts the
/// presented viewer has blocked by [`without_blocked`] (Pleroma's own
/// filter); the `Link` header is built from the unfiltered page so paging
/// never stalls on a page the filter emptied.
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
pub(super) async fn list_favourited_by(
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
        link_for_page(&state.config.base_url, uri.path(), &ids, |cursor| *cursor).as_deref(),
    )?;
    // The list names no relationship of the viewer to the favouriters;
    // Pleroma's `favourited_by` renders plain `show.json` accounts.
    // The `Link` above came from the raw page; the viewer's blocked
    // accounts leave only the rendered list.
    let entries = without_blocked(&state, auth.0.as_ref(), entries, |entry| entry.user.id).await?;
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
pub(super) async fn list_favourites(
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
        link_for_page(&state.config.base_url, uri.path(), &cursors, |cursor| {
            *cursor
        })
        .as_deref(),
    )?;
    Ok((
        headers,
        Json(render_statuses(&state, ViewerMode::user(&auth.0), &visible).await?),
    ))
}

/// Post-write hook of both favourite routes: the favourite telling of the
/// status's author — and its withdrawal, when a favourite is undone — runs
/// through here, landed from [`notify::on_favourite`] (Pleroma's
/// `CommonAPI.favorite` pipeline runs the same pair), and the federation
/// of the `Like` (and its `Undo`) lands from
/// [`federation_out::after_favourite_changed`]: the activity to send is
/// [`FavouriteRow::ap_id`], the pair it names is [`FavouriteRow::user_id`]
/// and [`FavouriteRow::status_id`], and the queueing happens through
/// `state` (decision 11). The inbound `Like` handler runs it too
/// ([`crate::federation::handlers::interactions`]): a mirrored remote like
/// tells the local author exactly as a local one does, and the like's
/// `Undo` takes that telling back — the federation half answering nothing
/// for it, since a mirror row belongs to its maker's instance.
///
/// `favouriting` states which way the write moved the fact: `true` for the
/// favourite a `POST …/favourite` left behind — a repeat favourite never
/// reaches this hook, since it changed nothing, which is why one like
/// tells once — and `false` for the `Undo` of the favourite a
/// `POST …/unfavourite` removed. A withdrawal that found no row to remove
/// never reaches it either.
///
/// [`notify::on_favourite`]: crate::services::notify::on_favourite
pub(crate) async fn after_favourite_changed(
    state: &AppState,
    favourite: &FavouriteRow,
    favouriting: bool,
) {
    notify::on_favourite(state, favourite, favouriting).await;
    federation_out::after_favourite_changed(state, favourite, favouriting).await;
}
