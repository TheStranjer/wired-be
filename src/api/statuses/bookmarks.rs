//! # The bookmark writes and the bookmarks listing
//!
//! `POST …/bookmark` and `POST …/unbookmark` are Pleroma's `bookmark`
//! and `unbookmark`: a user token ([`AuthUser`]) with `write:bookmarks`,
//! both answering the [`Status`] entity as the caller sees it —
//! `bookmarked` true after the write, false after the withdrawal, since
//! the row is re-read for the render and the renderer's batched
//! bookmarks read now names the caller. The bookmark write also takes a
//! `folder_id` (body or query): it must name one of the caller's folders
//! ([`crate::models::bookmark_folder`]) or the write is the shared 404,
//! and since [`bookmark::add`] restates the folder on a repeat,
//! re-bookmarking with a different folder *moves* the bookmark — and one
//! naming no folder frees it. Both writes ([`bookmark::add`] /
//! [`bookmark::remove`]) are idempotent on `(account, status)`: a repeat
//! bookmark is one row and one 200, and withdrawing a bookmark the caller
//! never made is the unchanged entity with 200. Bookmarking a status the
//! caller may not see is the shared 404. Unlike a like, a bookmark of a
//! reblog's id lands on *the reblog row itself* — Pleroma's
//! `Bookmark.create` takes the activity the client named, and the flag
//! renders off that same id, the folder with it
//! (`pleroma.bookmark_folder`). And unlike a like, a bookmark federates
//! nothing: it is the account's own private note, Pleroma sends no
//! activity for it, so these routes have no post-write hook to await.
//!
//! `GET /api/v1/bookmarks` is the caller's own list: [`AuthUser`] with
//! `read:bookmarks`, the statuses they bookmarked newest-BOOKMARK first,
//! [`PageParams`] again keyed on the bookmark row id (20 by default, 40
//! at most), and only the statuses still visible to them — the same
//! [`filter_visible`] step as the favourites listing. The optional
//! `folder_id` query parameter narrows the list to that folder's
//! bookmarks ([`bookmark::page_for_user`]); a folder that names none of
//! the caller's rows answers the empty page.
//!
//! [`Status`]: crate::entities::status::Status
//! [`filter_visible`]: crate::models::status_visibility::filter_visible
//! [`status_for_viewer_or_404`]: crate::api::helpers::status_for_viewer_or_404

use std::collections::HashSet;

use axum::{
    Json,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
};
use serde::Deserialize;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::status_for_viewer_or_404,
    auth::AuthUser,
    entities::status::Status,
    error::{AppError, AppResult},
    extract::ApiParams,
    models::{bookmark, bookmark_folder, status::Status as StatusRow, status_visibility},
    oauth::scopes::{READ_BOOKMARKS, WRITE_BOOKMARKS},
    pagination::{PageParams, set_link_header},
    services::render_status::{ViewerMode, render_statuses},
    state::AppState,
    util::id::parse_id,
};

use super::{current_entity, page_link};

/// The body or query of `POST /api/v1/statuses/{id}/bookmark`.
#[derive(Debug, Deserialize)]
pub(super) struct BookmarkParams {
    /// The folder to file the bookmark in; it must name one of the caller's
    /// folders or the write is the shared 404 — see [`requested_folder`].
    /// Absent (or JSON `null`) files the bookmark in no folder, which on a
    /// repeat frees it from whichever folder held it.
    #[serde(default)]
    folder_id: Option<String>,
}

/// The query of `GET /api/v1/bookmarks`.
#[derive(Debug, Deserialize)]
pub(super) struct BookmarksQuery {
    /// Narrow the page to one folder's bookmarks; absent (or malformed) asks
    /// for everything — see [`list_bookmarks`].
    #[serde(default)]
    folder_id: Option<String>,
}

/// `POST /api/v1/statuses/{id}/bookmark` — bookmark the status `id` and
/// answer it as the caller sees it: `bookmarked` true, because the row is
/// re-read for the render and the renderer's batched bookmarks read now
/// names the caller. A `folder_id` (body or query) files the bookmark in
/// that folder, and `pleroma.bookmark_folder` renders it back.
///
/// The status resolves through [`status_for_viewer_or_404`] — what the
/// caller may not see is the same 404 as what does not exist; a `folder_id`
/// that is not the caller's own is the same 404 ([`requested_folder`]). A
/// reblog's id bookmarks *the reblog row itself* (see the [module docs](self)),
/// and the write is idempotent on the pair, so a repeat bookmark is one row
/// and one 200 — its folder restated to whatever the repeat names.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:bookmarks` check; [`AppError::NotFound`] for a
/// malformed, unknown, deleted or invisible status, for the row a delete
/// took between the gate and the re-read, for the one row the render cannot
/// answer, and for a `folder_id` that is malformed, unknown or foreign. Any
/// database failure arrives through `?` as [`AppError::Database`].
pub(super) async fn bookmark_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiParams(params): ApiParams<BookmarkParams>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_BOOKMARKS)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    // A `folder_id` must name the caller's own folder: an unknown or foreign
    // id is the same 404 the status gate gives, and nothing is written. No
    // `folder_id` files the bookmark in no folder — and, since the write is
    // an upsert on the folder too, re-bookmarking with a different folder
    // moves it (or, with none, frees it).
    let folder_id = requested_folder(&state, auth.0.id, params.folder_id.as_deref()).await?;
    let bookmark = bookmark::add(&state.db, auth.0.id, row.id, folder_id).await?;
    debug!(
        status = %row.id,
        user = %auth.0.id,
        folder = bookmark.folder_id.is_some(),
        "status bookmarked"
    );
    current_entity(&state, &auth.0, row.id).await
}

/// The folder `GET /api/v1/bookmarks` filters by: absent or empty names no
/// folder (the whole list, the `folder_id=` spelling clients leave behind),
/// a well-formed id is the filter whatever it names — an unknown or foreign
/// folder holds none of the caller's bookmarks, and the narrowed walk
/// answers the empty page — and a malformed one is the page cursor's own
/// 400.
///
/// # Errors
///
/// [`AppError::BadRequest`] with `"Invalid id"` for a `folder_id` that is
/// present and not an id.
fn folder_filter(text: Option<&str>) -> AppResult<Option<Uuid>> {
    match text {
        None | Some("") => Ok(None),
        Some(id) => parse_id(id)
            .map(Some)
            .ok_or_else(|| AppError::BadRequest("Invalid id".to_owned())),
    }
}

/// The `folder_id` a bookmark write names, resolved to the caller's folder:
/// absent (or JSON `null`, which the extractor reads as absent) is no
/// folder; a present id must parse and must be a folder this `user_id`
/// owns, or the whole write is the shared 404 — Pleroma's
/// `BookmarkFolder.belongs_to_user?` check demotes a folder it cannot name
/// to no folder; the ticket sharpens that silence into the 404, so a client
/// can tell a filed bookmark from one that quietly was not.
///
/// # Errors
///
/// [`AppError::NotFound`] for a malformed, unknown or foreign folder id; any
/// database failure arrives through `?` as [`AppError::Database`].
async fn requested_folder(
    state: &AppState,
    user_id: Uuid,
    folder_id: Option<&str>,
) -> AppResult<Option<Uuid>> {
    let Some(text) = folder_id else {
        return Ok(None);
    };
    let Some(id) = parse_id(text) else {
        return Err(AppError::NotFound);
    };
    if bookmark_folder::get_owned(&state.db, user_id, id)
        .await?
        .is_none()
    {
        return Err(AppError::NotFound);
    }
    Ok(Some(id))
}

/// `POST /api/v1/statuses/{id}/unbookmark` — withdraw the caller's
/// bookmark of the status `id` and answer it as they see it, `bookmarked`
/// false.
///
/// The same gate and the same row the bookmark write names, and
/// [`bookmark::remove`] is content with nothing to delete: a withdrawal of
/// a bookmark that was never made (or already withdrawn) changes nothing
/// and is the unchanged [`Status`] with 200.
///
/// # Errors
///
/// Those of [`bookmark_status`]. Any database failure arrives through `?`
/// as [`AppError::Database`].
pub(super) async fn unbookmark_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_BOOKMARKS)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    if bookmark::remove(&state.db, auth.0.id, row.id)
        .await?
        .is_some()
    {
        debug!(
            status = %row.id,
            user = %auth.0.id,
            "bookmark withdrawn"
        );
    }
    current_entity(&state, &auth.0, row.id).await
}

/// `GET /api/v1/bookmarks` — the statuses the caller bookmarked, newest
/// bookmark first, one [`PageParams`] page at a time (20 by default, 40 at
/// most) with the `Link` header over the *bookmark row ids*, and narrowed
/// to what their eye still allows: the same [`filter_visible`] step as
/// [`list_favourites`].
///
/// The `folder_id` query parameter narrows the walk to one folder
/// ([`bookmark::page_for_user`]'s filter): the walk asks for `None` when no
/// folder is named, and for a *present* id it asks for that folder's
/// bookmarks alone. An id that is well formed but names nothing — unknown,
/// or another account's folder — matches none of the caller's rows and
/// answers the empty page; an empty `folder_id=` asks for nothing and is
/// the same as absent. A malformed folder id is the same 400 a malformed
/// cursor gets.
///
/// # Errors
///
/// [`AppError::Unauthorized`] from [`AuthUser`] without a valid user
/// token; [`AppError::Forbidden`] when the token does not grant
/// `read:bookmarks`; [`AppError::BadRequest`] from the page parameters
/// for a malformed cursor or from [`folder_filter`] for a malformed
/// `folder_id`. Any database failure arrives through `?` as
/// [`AppError::Database`], and [`set_link_header`] reports a `Link` value
/// that is not header-safe as [`AppError::Internal`].
pub(super) async fn list_bookmarks(
    State(state): State<AppState>,
    auth: AuthUser,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
    ApiParams(query): ApiParams<BookmarksQuery>,
) -> AppResult<(HeaderMap, Json<Vec<Status>>)> {
    auth.require(READ_BOOKMARKS)?;
    let folder_id = folder_filter(query.folder_id.as_deref())?;
    let pairs = bookmark::page_for_user(&state.db, auth.0.id, page, folder_id).await?;
    let rows: Vec<StatusRow> = pairs.iter().map(|(_, row)| row.clone()).collect();
    let visible = status_visibility::filter_visible(&state.db, rows, Some(&auth.0)).await?;
    let visible_ids: HashSet<Uuid> = visible.iter().map(|row| row.id).collect();
    // The cursors key on the bookmark, so they survive the visibility
    // step: only bookmarks whose status came back visible name the edges.
    let cursors: Vec<Uuid> = pairs
        .iter()
        .filter(|(bookmark, _)| visible_ids.contains(&bookmark.status_id))
        .map(|(bookmark, _)| bookmark.id)
        .collect();
    debug!(
        user = %auth.0.id,
        bookmarks = visible.len(),
        "bookmarks listed"
    );
    let mut headers = HeaderMap::new();
    set_link_header(
        &mut headers,
        page_link(&state.config.base_url, uri.path(), &cursors).as_deref(),
    )?;
    Ok((
        headers,
        Json(render_statuses(&state, ViewerMode::user(&auth.0), &visible).await?),
    ))
}
