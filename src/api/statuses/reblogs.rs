//! # The reblog writes and the rebloggers listing
//!
//! `POST …/reblog` and `POST …/unreblog` are Pleroma's `reblog` /
//! `unreblog` (`CommonAPI.repeat` / `unrepeat`): a user token
//! ([`AuthUser`]) with `write:statuses`, both routes riding the
//! `statuses_actions` throttle bucket like the favourite writes, and both
//! resolving the named status through the shared
//! [`status_for_viewer_or_404`] — the invisible, unknown and deleted are
//! the same 404 `{"error":"Record not found"}` as ever. The boost is a
//! row of [`crate::models::status`] with `reblog_of_id` set (decision 6),
//! and the mapping is the like's with the roles swapped: **a reblog row's
//! id reblogs (and unreblogs) its original** — nobody boosts a boost —
//! while `POST …/reblog` answers the *reblog entity*, the outer row with
//! the original nested and `reblogged: true`, and `POST …/unreblog`
//! answers the *original* with `reblogged: false`.
//!
//! Two refusals stand behind the gate, both 422 in the ticket's words: a
//! `visibility` parameter outside `public` / `unlisted` / `private`
//! (default `public`, the parameter naming the boost's own visibility,
//! Pleroma's `announce_visibility` trimmed to the three) is
//! `{"error":"Invalid visibility"}`, and a `private` or `direct` target —
//! even the caller's own `direct`, which the visibility model hands them
//! as visible — is `{"error":"Could not reblog private status"}`, since a
//! boost re-broadcasts what it boosts. Both writes are idempotent on the
//! partial unique index over `(user_id, reblog_of_id)`: a repeat reblog
//! restates the same row, count and 200, and a withdrawal that finds
//! nothing is the unchanged original with 200.
//!
//! `GET …/reblogged_by` is Pleroma's `reblogged_by`: optional auth (again
//! `read:statuses` on a presented user token), the same
//! [`status_for_viewer_or_404`] gate — with a reblog row's id answering
//! its original's list — and the answer an array of [`Account`] entities,
//! newest boost first, paged by [`PageParams`] keyed on the *reblog row
//! id* (the `Link` cursors carry boost ids, not account ids).
//!
//! A boost tells the original's author — the `after_reblog_changed` hook
//! lands [`crate::services::notify`]'s reblog telling (and its withdrawal),
//! Pleroma's `Announce` notification riding the same pipeline. The same
//! hook federates the write: [`crate::services::federation_out`] sends the
//! `Announce` the reblog row *is* (and its `Undo`), never for a boost of a
//! `local`-visibility original.
//!
//! [`Status`]: crate::entities::status::Status
//! [`Account`]: crate::entities::account::Account
//! [`status_for_viewer_or_404`]: crate::api::helpers::status_for_viewer_or_404

use axum::{
    Json,
    extract::{OriginalUri, Path, State},
    http::HeaderMap,
};
use serde::Deserialize;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::{retain_open_to_viewer, status_for_viewer_or_404},
    auth::{AuthUser, MaybeAuthUser},
    entities::{
        account::{Account, AccountViewer},
        status::Status,
    },
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        status::{self, Status as StatusRow, Visibility},
        status_visibility,
    },
    oauth::scopes::{READ_STATUSES, WRITE_STATUSES},
    pagination::{PageParams, link_for_page, set_link_header},
    services::{
        account_emojis::AccountEmojis,
        federation_out, notify,
        render_status::{ViewerMode, render_statuses},
    },
    state::AppState,
};

use super::{CANNOT_REBLOG, INVALID_VISIBILITY, action_target_id, current_entity, without_blocked};

/// `POST /api/v1/statuses/{id}/reblog` — reblog the status `id` and answer
/// the **reblog row's** entity: the outer [`Status`] whose `reblog` is the
/// original and whose `reblogged` is `true`, its nested original carrying
/// the stepped `reblogs_count`.
///
/// The gate is the shared [`status_for_viewer_or_404`] — Pleroma's
/// `CommonAPI.repeat` refuses what the actor may not see, and here the
/// invisible, the unknown and the deleted are the same 404 as ever. Behind
/// it Pleroma's `Announce` arm resolves through: **a reblog row's id
/// reblogs its original**, so nobody boosts a boost and the announcements
/// of a thread of boosts all land on the one post. The refusal the ticket
/// pins behind that is Pleroma's address rule: a `private` or `direct`
/// status — even the caller's own — answers 422 [`CANNOT_REBLOG`].
///
/// The body may name a [`visibility`] for the boost itself (`public`,
/// `unlisted` or `private`; the default is `public`); anything else —
/// `direct`, `local`, any other spelling — is 422 [`INVALID_VISIBILITY`]
/// before anything is written. The write is idempotent on
/// `(user_id, reblog_of_id)`: a repeat reblog is the same row, the same
/// count and the same entity with 200, and the post-write hook runs only
/// when the row was actually made.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:statuses` check; [`AppError::Unprocessable`] for a bad
/// `visibility` parameter or a `private`/`direct` target;
/// [`AppError::NotFound`] for a malformed, unknown, deleted or invisible
/// status, for an original that vanished under a reblog row mid-flight,
/// for the row a delete took between the gate and the re-read, and for the
/// one row the render cannot answer; the bucket's 429 before the handler
/// runs. Any database failure arrives through `?` as [`AppError::Database`].
pub(super) async fn reblog_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<ReblogParams>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_STATUSES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    let original = reblog_target(&state, &row).await?;
    let visibility = reblog_visibility(params.visibility.as_deref())?;
    if not_rebloggable(&original) {
        return Err(AppError::Unprocessable(CANNOT_REBLOG.to_owned()));
    }
    let (reblog, created) = status::create_reblog(
        &state.db,
        &state.config.base_url,
        &auth.0,
        &original,
        visibility,
    )
    .await?;
    if created {
        debug!(
            status = %original.id,
            user = %auth.0.id,
            visibility = visibility.as_str(),
            "status reblogged"
        );
        after_reblog_changed(&state, &reblog, true).await;
    }
    current_entity(&state, &auth.0, reblog.id).await
}

/// `POST /api/v1/statuses/{id}/unreblog` — withdraw the caller's reblog of
/// the status `id` and answer the **original's** [`Status`] entity,
/// `reblogged` false and `reblogs_count` stepped back, because the row is
/// re-read for the render and the renderer's batched reblogs read no longer
/// names it.
///
/// The same gate and the same reblog-row-through-to-original mapping as
/// [`reblog_status`], and [`status::remove_reblog`] is content with nothing
/// to delete: a withdrawal of a reblog that was never made (or already
/// withdrawn) changes nothing and is the unchanged original with 200 —
/// where Pleroma's `unrepeat` answers its error arm, the ticket gives the
/// restatement the same peace the unfavourite keeps.
///
/// # Errors
///
/// Those of [`reblog_status`] minus the two 422s this route has no reason
/// to ask: withdrawing needs no target visibility and no parameter. Any
/// database failure arrives through `?` as [`AppError::Database`].
pub(super) async fn unreblog_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_STATUSES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    let original_id = action_target_id(&row);
    if let Some(removed) = status::remove_reblog(&state.db, auth.0.id, original_id).await? {
        debug!(
            status = %original_id,
            user = %auth.0.id,
            "reblog withdrawn"
        );
        after_reblog_changed(&state, &removed, false).await;
    }
    current_entity(&state, &auth.0, original_id).await
}

/// `GET /api/v1/statuses/{id}/reblogged_by` — the [`Account`] entities
/// that reblogged the status `id`, newest boost first, one [`PageParams`]
/// page at a time with the `Link` header over the *reblog row ids*.
///
/// Auth is optional: a presented user token must grant `read:statuses` and
/// decides whose view the accounts render for, an anonymous request sees
/// the same list. The status resolves through [`status_for_viewer_or_404`],
/// so an invisible post hides its boosters behind the shared 404, and —
/// Pleroma's `reblogged_by` walking the original object's `announcements`
/// — a reblog row's id answers its original's list. Only `public` and
/// `unlisted` boosts are listed; a followers-only or direct boost never
/// appears, even to an anonymous caller who may see the original.
/// Deactivated accounts are left out by the query, and the accounts the
/// presented viewer has blocked by [`without_blocked`]; the `Link` header is
/// built from the unfiltered page so paging never stalls.
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
pub(super) async fn list_reblogged_by(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Account>>)> {
    auth.require_user_scope(READ_STATUSES)?;
    let row = status_for_viewer_or_404(&state, auth.0.as_ref(), &id).await?;
    let original_id = action_target_id(&row);
    let entries = status::reblogger_page(&state.db, original_id, page).await?;
    debug!(
        status = %original_id,
        rebloggers = entries.len(),
        "status rebloggers listed"
    );
    let ids: Vec<Uuid> = entries.iter().map(|entry| entry.reblog_id).collect();
    let mut headers = HeaderMap::new();
    set_link_header(
        &mut headers,
        link_for_page(&state.config.base_url, uri.path(), &ids, |cursor| *cursor).as_deref(),
    )?;
    // The list names no relationship of the viewer to the rebloggers;
    // Pleroma's `reblogged_by` renders plain `show.json` accounts.
    // The `Link` above came from the raw page; the viewer's blocked
    // accounts leave only the rendered list.
    let entries = without_blocked(&state, auth.0.as_ref(), entries, |entry| entry.user.id).await?;
    let viewer: AccountViewer<'_> = auth.0.as_ref();
    let users: Vec<_> = entries.iter().map(|entry| &entry.user).collect();
    let emojis = AccountEmojis::load(&state.db, &users).await?;
    let accounts = entries
        .iter()
        .map(|entry| {
            let account = Account::from_user_for(viewer, &entry.user, &state.config, None);
            emojis.fill(account, &entry.user, &state.config)
        })
        .collect();
    Ok((headers, Json(accounts)))
}

/// `GET /api/v1/statuses/{id}/quotes` (and its Pleroma twin, mounted by
/// [`pleroma_router`]) — the [`Status`] entities quoting the status `id`,
/// newest quoter first, one [`PageParams`] page at a time with the `Link`
/// header over the *quoter row ids*.
///
/// Auth is optional: a presented user token must grant `read:statuses` and
/// decides whose view the quoters render for, an anonymous request sees the
/// public and unlisted ones. The status resolves through
/// [`status_for_viewer_or_404`], so an invisible post hides its quoters
/// behind the shared 404; [`status::quoting_page`] answers the live quoting
/// rows newest-first and [`filter_visible`] then drops what this viewer may
/// not see — the same narrowing step as [`list_favourites`]'s, keeping the
/// page's order. An anonymous request further loses the quoters the
/// instance's `activities_*` restriction closes ([`retain_open_to_viewer`]).
/// Being a read, the route rides no throttle bucket.
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
pub(super) async fn list_quotes(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
    page: PageParams,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<Status>>)> {
    auth.require_user_scope(READ_STATUSES)?;
    let viewer = auth.0.as_ref();
    let target = status_for_viewer_or_404(&state, viewer, &id).await?;
    let rows = status::quoting_page(&state.db, target.id, page).await?;
    let visible = status_visibility::filter_visible(&state.db, rows, viewer).await?;
    let visible = retain_open_to_viewer(&state, viewer, visible).await?;
    debug!(
        status = %target.id,
        quoters = visible.len(),
        "status quoters listed"
    );
    // The cursors key on the quoter row itself, so the first and last
    // rendered rows name the page edges directly.
    let ids: Vec<Uuid> = visible.iter().map(|row| row.id).collect();
    let mut headers = HeaderMap::new();
    set_link_header(
        &mut headers,
        link_for_page(&state.config.base_url, uri.path(), &ids, |cursor| *cursor).as_deref(),
    )?;
    Ok((
        headers,
        Json(render_statuses(&state, ViewerMode::User(viewer), &visible).await?),
    ))
}

/// The body of `POST /api/v1/statuses/{id}/reblog`.
#[derive(Debug, Deserialize)]
pub(super) struct ReblogParams {
    /// The visibility to boost at; absent asks for [`Visibility::Public`]
    /// — see [`reblog_visibility`].
    #[serde(default)]
    visibility: Option<String>,
}

/// The [`Visibility`] `POST …/reblog` boosts at: the body's `visibility`,
/// or `public` when it names none. Only `public`, `unlisted` and `private`
/// are boost visibilities — the ticket's trim of Pleroma's
/// `announce_visibility`, which would also take `direct` and `local` — and
/// every other spelling is 422 [`INVALID_VISIBILITY`] rather than a quiet
/// default, so a client that meant `private` never learns its boost was
/// public by reading the entity back.
fn reblog_visibility(requested: Option<&str>) -> AppResult<Visibility> {
    match requested {
        None | Some("public") => Ok(Visibility::Public),
        Some("unlisted") => Ok(Visibility::Unlisted),
        Some("private") => Ok(Visibility::Private),
        Some(_) => Err(AppError::Unprocessable(INVALID_VISIBILITY.to_owned())),
    }
}

/// The status a reblog lands on: `row` itself, or its **original** when the
/// client named a reblog row — nobody boosts a boost, so the write folds to
/// the post underneath (Pleroma's `repeat` fetching the `Create` behind the
/// id, and the ticket's rule for this route). The row comes back live: rule
/// 7 of [`status_visibility`] admitted the named reblog only because this
/// original is visible to the same caller, and the one way this read finds
/// nothing is a delete in the window between the two — the shared 404.
///
/// # Errors
///
/// [`AppError::NotFound`] for an original that is no longer a live row;
/// any database failure arrives through `?` as [`AppError::Database`].
async fn reblog_target(state: &AppState, row: &StatusRow) -> AppResult<StatusRow> {
    match row.reblog_of_id {
        None => Ok(row.clone()),
        Some(original_id) => {
            let original = status::get_by_id(&state.db, original_id)
                .await?
                .ok_or(AppError::NotFound)?;
            Ok(original)
        }
    }
}

/// Whether `POST …/reblog` must refuse the target: a `private` or `direct`
/// status is addressed to followers or to the mentioned accounts alone,
/// and a boost re-broadcasts it to whoever sees the booster's timeline —
/// Pleroma's Announce of anything but a publicly-addressed object, which
/// the ticket refuses even when the caller is the author (the one case its
/// own visibility model hands back as visible). `public`, `unlisted` and —
/// this instance's extension — `local` pass.
const fn not_rebloggable(original: &StatusRow) -> bool {
    matches!(
        original.visibility,
        Visibility::Private | Visibility::Direct
    )
}

/// Post-write hook of both reblog routes: the federation of the
/// `Announce` activity (and its `Undo`, when a reblog is withdrawn) lands
/// here, through [`federation_out::after_reblog_changed`] — the activity
/// to send is the row's own [`uri`](StatusRow::uri) (a reblog row *is* the
/// `Announce`: decision 6 stores it under `{BASE_URL}/activities/{id}`),
/// the pair it names is the row's [`user_id`](StatusRow::user_id) and its
/// [`reblog_of_id`](StatusRow::reblog_of_id), and the queueing happens
/// through `state` (decision 11).
/// The telling of the original's author — Pleroma's `Announce`
/// notification rides the same `CommonAPI.repeat` pipeline — lands in the
/// same body, from [`notify::on_reblog`]. The inbound `Announce` handler
/// runs it too ([`crate::federation::handlers::interactions`]): a remote
/// boost tells the local author of the boosted post exactly as a local one
/// does, and the boost's `Undo` takes that telling back — the federation
/// half answering nothing, since a mirror boost belongs to its maker's
/// instance.
///
/// `reblog` is the row the write just created or tombstoned: on the
/// withdrawal it is the tombstone, whose `uri` the `Undo` must name.
///
/// `reblogging` states which way the write moved the fact: `true` for the
/// reblog a `POST …/reblog` left behind — a repeat reblog never reaches
/// this hook, since it changed nothing, which is why one boost tells once —
/// and `false` for the `Undo` of the reblog a `POST …/unreblog` removed. A
/// withdrawal that found no row to remove never reaches it either.
///
/// [`notify::on_reblog`]: crate::services::notify::on_reblog
pub(crate) async fn after_reblog_changed(state: &AppState, reblog: &StatusRow, reblogging: bool) {
    notify::on_reblog(state, reblog, reblogging).await;
    federation_out::after_reblog_changed(state, reblog, reblogging).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::statuses::test_support::row;

    /// The 422 `POST …/reblog` answers for an unusable `visibility`.
    fn invalid_visibility(err: &AppError) -> bool {
        matches!(err, AppError::Unprocessable(message) if message == INVALID_VISIBILITY)
    }

    #[test]
    fn the_visibility_parameter_admits_three_boosts_and_defaults_public() {
        for (asked, wanted) in [
            (None, Visibility::Public),
            (Some("public"), Visibility::Public),
            (Some("unlisted"), Visibility::Unlisted),
            (Some("private"), Visibility::Private),
        ] {
            assert_eq!(
                reblog_visibility(asked).ok(),
                Some(wanted),
                "`{asked:?}` boosts at {wanted}"
            );
        }
    }

    #[test]
    fn any_other_visibility_parameter_is_refused() {
        for asked in [
            Some("direct"),
            Some("local"),
            Some(""),
            Some("Public"),
            Some("unlist"),
        ] {
            let err =
                reblog_visibility(asked).expect_err(&format!("`{asked:?}` is no boost visibility"));
            assert!(
                invalid_visibility(&err),
                "the refusal is 422 Invalid visibility, not {err}"
            );
        }
    }

    #[test]
    fn a_private_or_direct_target_refuses_the_boost() {
        for visibility in Visibility::ALL {
            let mut target = row(Uuid::now_v7());
            target.visibility = visibility;
            assert_eq!(
                not_rebloggable(&target),
                matches!(visibility, Visibility::Private | Visibility::Direct),
                "the boost refuses exactly `private` and `direct`, not `{visibility}`"
            );
        }
    }
}
