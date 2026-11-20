//! # The context
//!
//! `GET /api/v1/statuses/{id}/context` is Pleroma's `context`
//! (`StatusController.context` over `ActivityPub.fetch_activities_for_context`):
//! the same optional auth and scope as the show route, the same
//! [`status_for_viewer_or_404`] gate on the named status, and the answer
//! `{"ancestors": […], "descendants": […]}` of [`Status`] entities.
//! *Ancestors* are the walk up the named status's `in_reply_to_id` chain —
//! at most [`MAX_CONTEXT_DEPTH`] hops, root first, the named status never —
//! and *descendants* are the walk down through every reply of it, across
//! every branch — at most [`MAX_CONTEXT_DEPTH`] hops and
//! [`MAX_CONTEXT_DESCENDANTS`] rows, in flat chronological thread order
//! (id ascending), the named status never. Both walks run raw and both
//! answers run through [`filter_visible`], so an invisible row is *skipped
//! but walked past*: it drops out of the arrays and the thread on the far
//! side of it still arrives. Descendants lose one more class: statuses by
//! accounts the viewer has blocked or muted, unless the author is the
//! viewer. Each descendant's `pleroma.parent_visible` says whether its own
//! direct parent actually came back in this answer (the named status
//! itself counting as come back), so a client can tell a continuous stretch
//! of thread from one with a hole in it. Both arrays render through the
//! shared [`render_statuses`] batch.
//!
//! [`Status`]: crate::entities::status::Status
//! [`filter_visible`]: crate::models::status_visibility::filter_visible
//! [`status_for_viewer_or_404`]: crate::api::helpers::status_for_viewer_or_404

use std::collections::HashSet;

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Serialize;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::status_for_viewer_or_404,
    auth::MaybeAuthUser,
    entities::{account::JsonBool, status::Status},
    error::AppResult,
    models::{
        block, mute,
        status::{self, Status as StatusRow},
        status_visibility,
        user::User,
    },
    oauth::scopes::READ_STATUSES,
    services::{
        filter::{self, FilterContext},
        render_status::{ViewerMode, render_statuses},
    },
    state::AppState,
};

use super::{MAX_CONTEXT_DEPTH, MAX_CONTEXT_DESCENDANTS};

/// `GET /api/v1/statuses/{id}/context` — the thread around the status `id`:
/// `{"ancestors": [...], "descendants": [...]}` as the viewer behind an
/// optional token may see it.
///
/// The status resolves through [`status_for_viewer_or_404`] like every
/// other route keyed by id (Pleroma's `context` likewise 404s what
/// `get_by_id` cannot find), and both walks then run from that row's id:
/// [`status::ancestor_chain`] up its `in_reply_to_id` links to the root and
/// [`status::descendant_tree`] down through every reply, both capped at
/// [`MAX_CONTEXT_DEPTH`] hops and the descent additionally at
/// [`MAX_CONTEXT_DESCENDANTS`] rows. Neither walk is filtered as it goes —
/// an invisible link is *skipped but walked past* — and [`filter_visible`]
/// narrows each raw chain to what the viewer may see afterwards, keeping
/// the order the walk produced: ancestors root-first (oldest first) and
/// descendants in flat chronological thread order, exactly like Pleroma's
/// context read. Descendants additionally drop what the viewer's own blocks
/// and mutes silence ([`is_silenced`]), the named status's id counts as
/// shown, and each descendant's `pleroma.parent_visible` is then set to
/// whether its own direct parent came back in this answer
/// ([`mark_invisible_parents`]). Both arrays render through the shared
/// [`render_statuses`] batch and then pass through
/// [`apply_active`](crate::services::filter::apply_active) in the `thread`
/// filter context — the viewer's warning matches arrive annotated, their
/// irreversible matches leave the arrays, and a parent one of those matches
/// removed is what the then-computed `parent_visible` reads as absent.
/// The named status itself is not filtered here: it is the read's anchor,
/// and the single-status route is the filter contexts' business, not this
/// one's.
///
/// # Errors
///
/// [`AppError::Forbidden`] when a presented user token lacks
/// `read:statuses`; [`AppError::Unauthorized`] from the extractor for a
/// token that does not resolve; [`AppError::NotFound`] for a malformed,
/// unknown, deleted or invisible status. Any database failure arrives
/// through `?` as [`AppError::Database`].
pub(super) async fn get_status_context(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<StatusContext>> {
    auth.require_user_scope(READ_STATUSES)?;
    let viewer = auth.0.as_ref();
    let row = status_for_viewer_or_404(&state, viewer, &id).await?;
    let ancestor_rows = visible_ancestors(&state, viewer, row.id).await?;
    let descendant_rows = visible_descendants(&state, viewer, row.id).await?;
    let rendered_ancestors =
        render_statuses(&state, ViewerMode::User(viewer), &ancestor_rows).await?;
    let rendered_descendants =
        render_statuses(&state, ViewerMode::User(viewer), &descendant_rows).await?;
    let ancestors =
        filter::apply_active(&state, viewer, rendered_ancestors, FilterContext::Thread).await?;
    let mut descendants =
        filter::apply_active(&state, viewer, rendered_descendants, FilterContext::Thread).await?;
    mark_invisible_parents(&mut descendants, &ancestors, row.id);
    debug!(
        status = %row.id,
        ancestors = ancestors.len(),
        descendants = descendants.len(),
        "status context read"
    );
    Ok(Json(StatusContext {
        ancestors,
        descendants,
    }))
}

/// The ancestors array's rows: the raw chain above `status_id` (root
/// first), narrowed to what `viewer` may see. The narrowing drops rows
/// without breaking the chain the walk already finished, so the thread
/// beyond an invisible link still arrives.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn visible_ancestors(
    state: &AppState,
    viewer: Option<&User>,
    status_id: Uuid,
) -> AppResult<Vec<StatusRow>> {
    let chain = status::ancestor_chain(&state.db, status_id, MAX_CONTEXT_DEPTH).await?;
    status_visibility::filter_visible(&state.db, chain, viewer).await
}

/// The descendants array's rows: the raw tree below `status_id` (id order),
/// narrowed to what `viewer` may see and then to what their eye still
/// allows. The second step is the one the visibility model does not carry:
/// [`filter_visible`] shuts out an author who blocked the *viewer* (its
/// rule 2), but statuses by accounts the *viewer* blocked or muted are a
/// timeline courtesy, and Pleroma's context read applies it too
/// (`restrict_blocked` over its `blocking_user`) — with the author's own
/// exemption of [`is_silenced`]. An anonymous viewer blocks and mutes
/// nobody, so their answer is the visible tree entire.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn visible_descendants(
    state: &AppState,
    viewer: Option<&User>,
    status_id: Uuid,
) -> AppResult<Vec<StatusRow>> {
    let tree = status::descendant_tree(
        &state.db,
        status_id,
        MAX_CONTEXT_DEPTH,
        MAX_CONTEXT_DESCENDANTS,
    )
    .await?;
    let visible = status_visibility::filter_visible(&state.db, tree, viewer).await?;
    let Some(viewer) = viewer else {
        return Ok(visible);
    };
    let silenced = silenced_authors(state, viewer.id).await?;
    Ok(visible
        .into_iter()
        .filter(|row| !is_silenced(row, viewer.id, &silenced))
        .collect())
}

/// The accounts `viewer_id` has silenced: their blocks unioned with their
/// still-live mutes (an expired mute is no row to [`mute::muted_ids`], so
/// its author is back in the thread), the two reads' lists merged into one
/// membership set.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn silenced_authors(state: &AppState, viewer_id: Uuid) -> AppResult<HashSet<Uuid>> {
    let blocked = block::blocked_ids(&state.db, viewer_id).await?;
    let muted = mute::muted_ids(&state.db, viewer_id).await?;
    Ok(blocked.into_iter().chain(muted).collect())
}

/// Whether `row` is silenced out of the descendants array: its author is in
/// `silenced` and is not the viewer — one's own posts are never hidden from
/// oneself, whatever the edge tables may one day hold.
fn is_silenced(row: &StatusRow, viewer_id: Uuid, silenced: &HashSet<Uuid>) -> bool {
    row.user_id != viewer_id && silenced.contains(&row.user_id)
}

/// Rewrites every descendant's `pleroma.parent_visible` to say whether its
/// own direct parent actually reached this answer: the shown set is the two
/// arrays plus the named status itself (the caller has it, so it is the one
/// parent that can never be the gap). Pleroma computes the same fact with
/// `visible_for_user?` per activity; here the walks already ran, so the
/// answer is membership. A descendant answering a row that was skipped —
/// invisible, silenced, or a tombstone the walk passed through — says
/// `false` so the client can draw the break.
fn mark_invisible_parents(descendants: &mut [Status], ancestors: &[Status], context_id: Uuid) {
    let shown: HashSet<String> = std::iter::once(context_id.to_string())
        .chain(ancestors.iter().map(|status| status.id.clone()))
        .chain(descendants.iter().map(|status| status.id.clone()))
        .collect();
    for status in descendants {
        let parent_shown = status
            .in_reply_to_id
            .as_deref()
            .is_some_and(|parent| shown.contains(parent));
        status.pleroma.parent_visible = JsonBool(parent_shown);
    }
}

/// The answer of `GET /api/v1/statuses/{id}/context`: the thread around the
/// named status as its viewer may see it, both halves rendered
/// [`Status`] entities.
#[derive(Debug, Serialize)]
pub(super) struct StatusContext {
    /// Everything above the named status, root first — its `in_reply_to_id`
    /// chain, narrowed to what the viewer may see, the named status
    /// excluded.
    ancestors: Vec<Status>,
    /// Everything below it, every branch, in flat chronological thread
    /// order (id ascending) — narrowed the same way and then further by
    /// the viewer's blocks, mutes and the [`MAX_CONTEXT_DESCENDANTS`]
    /// row cap, the named status excluded. Each entry's
    /// `pleroma.parent_visible` says whether its direct parent is in this
    /// answer.
    descendants: Vec<Status>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::statuses::test_support::row;

    /// The descendants array's silencing step: rows by an author the viewer
    /// blocked or muted drop, the viewer's own rows stay even when the set
    /// (defensively) names them, and everybody else's stay.
    #[test]
    fn only_silenced_other_authors_leave_the_descendants() {
        let (viewer, silenced_author, other) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let silenced: HashSet<Uuid> = [silenced_author, viewer].into_iter().collect();

        let mut theirs = row(Uuid::now_v7());
        theirs.user_id = silenced_author;
        assert!(
            is_silenced(&theirs, viewer, &silenced),
            "a post by an account the viewer silenced"
        );

        theirs.user_id = viewer;
        assert!(
            !is_silenced(&theirs, viewer, &silenced),
            "the viewer's own post stays even when the set names them"
        );

        let mut foreign = row(Uuid::now_v7());
        foreign.user_id = other;
        assert!(
            !is_silenced(&foreign, viewer, &silenced),
            "an author nobody silenced stays"
        );
    }
}
