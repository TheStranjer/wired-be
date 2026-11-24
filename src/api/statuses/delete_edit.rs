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
//! What leaves this instance is the withdrawal: a deleted REBLOG row
//! federates the `Undo` of the `Announce` it was and a deleted plain status
//! its `Delete` — the reserved object id as a `Tombstone`, under the
//! addressing the post travelled with — both attached at the deletion
//! service's `after_status_deleted` hook
//! ([`crate::services::delete_status`] — the same landing point the
//! expiration jobs run through), where the notification ticket has wired
//! the cleanup that takes the deleted status's notifications with it and
//! the streaming ticket pushes the `delete` to exactly the timelines that
//! carried the post.
//!
//! # The edit
//!
//! `PUT /api/v1/statuses/{id}` is Pleroma's `update`
//! (`StatusController.update` over `CommonAPI.update`, whose
//! `make_update_data` re-drafts the post from the submitted parameters on
//! top of the stored object): a user token with `write:statuses`, the
//! shared [`status_for_viewer_or_404`] gate first — the malformed, unknown,
//! deleted and invisible are the module's one 404 — and then the ownership
//! rules, in Pleroma's order: only the author's own **local** post is
//! editable (a mirrored remote post is nobody's to rewrite here, and
//! Pleroma's `Create`-type arm admits no reblog row as its subject), which
//! this route answers with the 403 twin of the `"Record not found"` body
//! the delete route makes; the reblog of one's own post is the edit's one
//! 422, `"Cannot edit a reblog"`, because a boost carries no text of its
//! own to revise. The body — `status`, `spoiler_text`, `sensitive`,
//! `language`, `content_type`, `media_ids`, and Mastodon's
//! `media_attributes` (`[{id, description, focus}]`, the way clients fix alt
//! text and focal points of attached media, written in the same
//! transaction so the federated `Update` carries them) — is merged over the stored row
//! by [`crate::services::create_status::edit_status`] and pushed through
//! the creation's own checks and render (the shared
//! [`render_status_fields`](crate::services::create_status::render_status_fields)),
//! so only the parameters the body presents change and every refusal is
//! the one creation would have answered the submitted value with. The
//! write ([`status_edit::update_status`]) is one transaction that records
//! the superseded version — only when the content, warning, sensitive flag
//! or media actually changed, so an edit of nothing writes no revision and
//! marks nothing — stamps `edited_at`, and replaces the post's mentions,
//! hashtags and attachments. The answer is the [`Status`] entity as the
//! author sees the edited post, `edited_at` set. The route's
//! `after_status_edited` hook already lands
//! [`crate::services::notify`]'s `update` telling of the post's local
//! boosters, the `Update` activity that re-drafts the post for everyone
//! else, and the `status.update` stream push.
//!
//! [`Status`]: crate::entities::status::Status
//! [`status_for_viewer_or_404`]: crate::api::helpers::status_for_viewer_or_404
//! [`soft_delete_with_reblogs`]: crate::models::status::soft_delete_with_reblogs
//! [`status_edit::update_status`]: crate::models::status_edit::update_status

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Serialize;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::status_for_viewer_or_404,
    auth::AuthUser,
    entities::status::Status,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{status::Status as StatusRow, user::User},
    oauth::scopes::WRITE_STATUSES,
    services::{
        create_status::{self, EditStatusParams},
        federation_out, notify,
        render_status::{ViewerMode, render_status},
        streaming,
    },
    state::AppState,
};

use super::{CANNOT_EDIT_REBLOG, RECORD_NOT_FOUND};

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
/// [`crate::services::delete_status`] turns that row and every live reblog of
/// it into a tombstone; the whole write is one transaction, so a caller that
/// gets a 200 never meets a half-deleted post. A status that another delete
/// took between the gate and the write answers the 404, as does the one row
/// the renderer cannot answer. The expiration jobs run the same service —
/// an expiring post leaves exactly what this route leaves.
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
pub(super) async fn delete_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<DeletedStatus>> {
    auth.require(WRITE_STATUSES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    if !may_delete(&auth.0, &row) {
        return Err(AppError::Forbidden(RECORD_NOT_FOUND.to_owned()));
    }
    let entity = render_status(&state, ViewerMode::user(&auth.0), &row)
        .await?
        .ok_or(AppError::NotFound)?;
    if !crate::services::delete_status::delete_status(&state, &row).await? {
        return Err(AppError::NotFound);
    }
    debug!(
        user = %auth.0.id,
        staff = auth.0.is_admin.get() || auth.0.is_moderator.get(),
        "status deleted over HTTP"
    );
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
pub(super) struct DeletedStatus {
    /// The pre-delete entity, its keys at the top level of the response.
    #[serde(flatten)]
    entity: Status,
    /// The status's stored source text ([`raw_text`](StatusRow::raw_text)) as
    /// it was before the delete; empty for a reblog, which carries none.
    text: String,
}

/// `PUT /api/v1/statuses/{id}` — edit the status `id` and answer the
/// [`Status`] entity as its author now sees it, `edited_at` set when the
/// edit superseded a version.
///
/// The gate is the shared [`status_for_viewer_or_404`] and then
/// [`edit_refusal`]: the author's own local post only (403 `"Record not
/// found"` for anything else that is visible), and never a reblog row
/// (422 [`CANNOT_EDIT_REBLOG`]). What the body presents,
/// [`create_status::edit_status`] merges over the stored row and runs
/// through the creation's own checks and render — so every 422 is the one
/// creation answers the submitted value with — and one transaction records
/// the superseded version, writes the new state and replaces the post's
/// mentions, hashtags and attachments.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:statuses` check; [`AppError::NotFound`] for a malformed,
/// unknown, deleted or invisible status, and for one a delete took between
/// the gate and the write; [`AppError::Forbidden`] for a visible status
/// that is not the caller's own or not local; [`AppError::Unprocessable`]
/// for the reblog row and for every merged-body refusal
/// [`create_status::edit_status`] documents. Any database failure arrives
/// through `?` as [`AppError::Database`].
pub(super) async fn edit_status(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<EditStatusParams>,
) -> AppResult<Json<Status>> {
    auth.require(WRITE_STATUSES)?;
    let row = status_for_viewer_or_404(&state, Some(&auth.0), &id).await?;
    if let Some(refusal) = edit_refusal(auth.0.id, &row) {
        return Err(refusal);
    }
    let entity = create_status::edit_status(&state, &auth.0, &row, params)
        .await?
        .ok_or(AppError::NotFound)?;
    after_status_edited(&state, &row).await;
    Ok(Json(entity))
}

/// Why `PUT /api/v1/statuses/{id}` refuses `row` for the account
/// `caller_id`, or `None` to let the edit through: behind the shared
/// visibility gate (which already answered the invisible, the unknown and
/// the deleted with the 404) stand Pleroma's ownership arms — the post must
/// be the caller's own (`own_status`) and this instance's own copy of it,
/// since a mirrored remote row belongs to its origin's `Object.edit`
/// pipeline and not to here — and its type arm: a reblog row is not a
/// `Create`, and decision 6 gives it no text of its own to revise. The two
/// refusals keep their ticket's bodies: the 403 twin of the shared
/// `"Record not found"` wording for the ownership one (so the route never
/// says whose post it thinks this is, nor that a remote one exists), the
/// 422 for the reblog.
fn edit_refusal(caller_id: Uuid, row: &StatusRow) -> Option<AppError> {
    if !row.local || row.user_id != caller_id {
        return Some(AppError::Forbidden(RECORD_NOT_FOUND.to_owned()));
    }
    row.reblog_of_id
        .is_some()
        .then(|| AppError::Unprocessable(CANNOT_EDIT_REBLOG.to_owned()))
}

/// Post-write hook of the edit route: the `update` telling of every local
/// account still boosting the edited post — Pleroma's `Update` arm of
/// `get_potential_receiver_ap_ids`, the object's local `Announce` actors
/// minus the author (`lib/pleroma/notification.ex`) — is landed here from
/// [`notify::on_status_edited`], the federated re-draft of the post
/// itself — the `Update` activity carrying the new note, addressed as the
/// status is — from [`federation_out::after_status_edited`], and the
/// `status.update` stream push — Pleroma's `CommonAPI.update` tail
/// through its `Streamer`, redrawing the post on every socket that
/// carried it — from [`streaming::status_edited`].
///
/// The federated edit handler runs it too
/// ([`crate::federation::handlers::note`]): an inbound `Update` of a
/// mirrored status is an edit like any other, and the boosters of the post
/// are told from either door alike. The announcement of the edit is the
/// local door's alone: a mirrored row belongs to its origin, and
/// [`federation_out::after_status_edited`] says nothing for one.
///
/// `row` is the pre-edit stored row: the id every consumer names — the
/// write keeps it — and the author whose boosters the telling reads off
/// the live reblog rows. The federation arm needs no field of this
/// snapshot but that id: it re-reads the row the write left behind, since
/// the document it announces is the post as edited. The stream push needs
/// only the fields an edit never moves — author, visibility, thread,
/// locality, the reblog link a boost alone carries — and reads the post's
/// live mentions, hashtags and attachments through the target
/// computation, so the answer is the row the write stored however the
/// body rewrote the text.
///
/// [`notify::on_status_edited`]: crate::services::notify::on_status_edited
/// [`streaming::status_edited`]: crate::services::streaming::status_edited
pub(crate) async fn after_status_edited(state: &AppState, row: &StatusRow) {
    notify::on_status_edited(state, row).await;
    federation_out::after_status_edited(state, row).await;
    streaming::status_edited(state, row).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::statuses::test_support::authored;

    /// The 403 `PUT …` answers for a status that is visible but not the
    /// caller's own local post: the shared `"Record not found"` wording.
    fn forbidden(refusal: Option<AppError>) -> bool {
        matches!(
            refusal,
            Some(AppError::Forbidden(message)) if message == RECORD_NOT_FOUND
        )
    }

    /// The 422 `PUT …` answers for a reblog row of the caller's own.
    fn reblog_refused(refusal: Option<AppError>) -> bool {
        matches!(
            refusal,
            Some(AppError::Unprocessable(message)) if message == CANNOT_EDIT_REBLOG
        )
    }

    #[test]
    fn your_own_local_post_is_the_one_an_edit_admits() {
        let owner = Uuid::now_v7();
        assert!(
            edit_refusal(owner, &authored(Uuid::now_v7(), owner)).is_none(),
            "a local post of yours is editable"
        );
        let mut answer = authored(Uuid::now_v7(), owner);
        answer.in_reply_to_id = Some(Uuid::now_v7());
        assert!(
            edit_refusal(owner, &answer).is_none(),
            "and so is a reply of yours"
        );
    }

    #[test]
    fn a_foreign_or_remote_post_is_forbidden_in_the_shared_wording() {
        let (owner, caller) = (Uuid::now_v7(), Uuid::now_v7());
        assert!(
            forbidden(edit_refusal(caller, &authored(Uuid::now_v7(), owner))),
            "a visible post of somebody else's is the 403 twin of the 404"
        );
        let mut mirrored = authored(Uuid::now_v7(), caller);
        mirrored.local = false;
        assert!(
            forbidden(edit_refusal(caller, &mirrored)),
            "a row this instance does not own is nobody's to edit here"
        );
    }

    #[test]
    fn a_reblog_refuses_the_edit_with_its_own_words_after_ownership() {
        let owner = Uuid::now_v7();
        let mut boost = authored(Uuid::now_v7(), owner);
        boost.reblog_of_id = Some(Uuid::now_v7());
        assert!(
            reblog_refused(edit_refusal(owner, &boost)),
            "your own boost has no text of its own to revise"
        );
        assert!(
            forbidden(edit_refusal(Uuid::now_v7(), &boost)),
            "and somebody else's boost is not yours before it is a boost"
        );
    }
}
