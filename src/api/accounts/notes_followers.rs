//! The private note and the follower removal:
//! `POST /api/v1/accounts/{id}/note` and `…/{id}/remove_from_followers`,
//! both answered with the caller's relationship rendered off the row
//! ([`one_relationship`]).
//!
//! `POST /api/v1/accounts/{id}/note` is Pleroma's `AccountController.note`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`) over
//! `Pleroma.UserNote` (`lib/pleroma/user_note.ex`): [`AuthUser`] plus the
//! `write:accounts` scope, and the whole answer is the caller's
//! [`Relationship`] to the account with [`note`](Relationship::note)
//! carrying what the write left stored. The `comment` parameter is
//! required, at most 2000 characters (Mastodon's `AccountNote` limit; longer is 422 with Mastodon's changeset wording)
//! and stored verbatim — but an empty or all-whitespace one *deletes* the
//! note (the answered `note` is then `""`), which is how Mastodon clears
//! it too. An unknown, malformed or deactivated account id is the same
//! 404 as every other route here; noting *yourself* is allowed, unlike
//! Pleroma's follow-side checks.
//!
//! `POST /api/v1/accounts/{id}/remove_from_followers` is Pleroma's
//! `AccountController.remove_from_followers` over `User.remove_follower`
//! (`lib/pleroma/web/mastodon_api/controllers/account_controller.ex`,
//! `lib/pleroma/user.ex`): [`AuthUser`] plus the `follow` / `write:follows`
//! scope, and the write takes the account's accepted follow of the caller
//! out of the table through [`remove_follower`](crate::models::follow::remove_follower),
//! counters adjusted in the same transaction. The answer is the caller's
//! [`Relationship`] to the account, so `followed_by` reads `false`
//! afterwards. An account that is not a follower — no row, a `pending`
//! request of its own (those wait on the `/api/v1/follow_requests` answers,
//! this route never touches them), or one the caller follows only — is no
//! error: the unchanged [`Relationship`] answers it with 200. The malformed,
//! unknown or deactivated id is the usual 404, resolved ahead of the guard
//! that answers the caller's *own* id with 422 `{"error":"Can not remove
//! yourself from followers"}`. A follower actually removed is the moment
//! federation answers that account a `Reject` for its `Follow`: this
//! module's `after_follower_removed` hook sends it, through the delivery
//! queue, to a REMOTE follower only.
//!

use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use tracing::debug;
use uuid::Uuid;

use crate::{
    api::helpers::visible_account_or_404,
    auth::AuthUser,
    entities::relationship::Relationship,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        account_note,
        follow::{self, Follow},
        user::User,
    },
    oauth::scopes::{FOLLOW, WRITE_ACCOUNTS, WRITE_FOLLOWS},
    services::federation_out,
    state::AppState,
};

/// The longest `comment` `POST /api/v1/accounts/{id}/note` accepts
/// (Mastodon's `AccountNote` limit).
const MAX_COMMENT_CHARS: usize = 2000;

/// The 422 message `POST /api/v1/accounts/{id}/remove_from_followers`
/// answers when the path names the caller: this endpoint's ticket pins the
/// wording for the action.
const SELF_REMOVE: &str = "Can not remove yourself from followers";

/// The request parameters of `POST /api/v1/accounts/{id}/note`: Pleroma's
/// `%{comment: comment}` pattern match — the field is required (an absent
/// one is the extractor's 422), and the empty string is the documented
/// delete, so the type carries no default.
#[derive(Debug, Deserialize)]
pub(super) struct NoteParams {
    /// The note text, verbatim up to [`MAX_COMMENT_CHARS`] characters;
    /// empty or all whitespace clears the note.
    comment: String,
}

/// `POST /api/v1/accounts/{id}/note` — write the caller's private note on
/// the account and answer their [`Relationship`] to it, with
/// [`note`](Relationship::note) carrying what the write left stored.
///
/// [`account_note::set`] is the write: a non-blank `comment` replaces the
/// caller's note on the target, a blank one deletes it (Pleroma's
/// `UserNote.create` stores blanks, this endpoint's ticket deletes on
/// them; both read back as the same relationship). The account is resolved
/// by [`visible_account_or_404`], and noting yourself is allowed — unlike
/// Pleroma's follow-side checks, `note` has no self guard. The
/// [`Relationship::load_many`] after the write is the render of Pleroma's
/// `relationship.json`: it reads the row back, so the answer states the
/// stored fact rather than trusting the request.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::Unprocessable`] when
/// `comment` is absent or longer than [`MAX_COMMENT_CHARS`] characters;
/// [`AppError::NotFound`] for a malformed, unknown or deactivated account
/// id. Any database failure arrives through `?` as [`AppError::Database`].
pub(super) async fn set_note(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    ApiBody(params): ApiBody<NoteParams>,
) -> AppResult<Json<Relationship>> {
    auth.require(WRITE_ACCOUNTS)?;
    // Validation before the lookup, as the list endpoints do: a
    // too-long comment is the 422 whatever the id names.
    if params.comment.chars().count() > MAX_COMMENT_CHARS {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: Comment is too long (maximum is {MAX_COMMENT_CHARS} characters)"
        )));
    }
    let target = visible_account_or_404(&state, &id, Some(&auth.0)).await?;
    account_note::set(&state.db, auth.0.id, target.id, &params.comment).await?;
    one_relationship(&state, auth.0.id, target.id).await
}

/// `POST /api/v1/accounts/{id}/remove_from_followers` — drop the account
/// from the caller's followers: the accepted follow of the caller it holds
/// is removed ([`remove_follower`](crate::models::follow::remove_follower),
/// counters adjusted in the same transaction) and the caller's
/// [`Relationship`] to it answered, `followed_by: false` afterwards.
///
/// Not being a follower is not an error: an account with no row, one whose
/// follow of the caller is still `pending` (requests are answered through
/// `/api/v1/follow_requests`, never removed here) or one the caller follows
/// without being followed back changes nothing, and the unchanged
/// [`Relationship`] answers the call with 200. The account is resolved
/// before the self guard, as Pleroma's plug order does: a malformed,
/// unknown or deactivated id is the shared 404 whatever it would have
/// meant, and only a live account that turns out to be the caller is the
/// 422.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:follows | follow` check; [`AppError::NotFound`] for a
/// malformed, unknown or deactivated account id; [`AppError::Unprocessable`]
/// with [`SELF_REMOVE`] when the id names the caller. Any database failure
/// arrives through `?` as [`AppError::Database`].
pub(super) async fn remove_from_followers(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> AppResult<Json<Relationship>> {
    auth.require_any(&[WRITE_FOLLOWS, FOLLOW])?;
    let follower = visible_account_or_404(&state, &id, Some(&auth.0)).await?;
    if follower.id == auth.0.id {
        return Err(AppError::Unprocessable(SELF_REMOVE.to_owned()));
    }
    let removed = follow::remove_follower(&state.db, auth.0.id, follower.id).await?;
    if let Some(row) = removed.as_ref() {
        debug!(
            follow = %row.id,
            user = %auth.0.id,
            "follower removed"
        );
        after_follower_removed(&state, &auth.0, row).await;
    }
    one_relationship(&state, auth.0.id, follower.id).await
}

/// The caller's [`Relationship`] to one account, rendered after a write:
/// [`Relationship::load_many`] over the single target, so the answer states
/// the stored fact rather than trusting the request (the write-behind read
/// of Pleroma's `relationship.json`). Shared by [`set_note`] and
/// [`remove_from_followers`].
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`]; the
/// [`AppError::Internal`] arm only documents that the loader answers one row
/// per requested id, which a single-target batch always does.
pub(super) async fn one_relationship(
    state: &AppState,
    viewer_id: Uuid,
    target_id: Uuid,
) -> AppResult<Json<Relationship>> {
    let loaded = Relationship::load_many(&state.db, viewer_id, &[target_id]).await?;
    let relationship = loaded.into_iter().next().ok_or_else(|| {
        AppError::Internal("the relationship loader skipped its only target".to_owned())
    })?;
    Ok(Json(relationship))
}

/// Post-removal hook of [`remove_from_followers`], run once per actually
/// removed follower: the write's remote half lands in
/// [`crate::services::federation_out`], which answers a dropped REMOTE
/// follower with the `Reject` of its `Follow` — the activity rejected is
/// [`Follow::ap_id`], its sender [`Follow::follower_id`]'s account, and the
/// queueing happens through `state` (decision 11). A local follower is
/// settled in the database and hears nothing; the hook never fails the
/// request.
async fn after_follower_removed(state: &AppState, followee: &User, removed: &Follow) {
    federation_out::after_follower_removed(state, followee, removed).await;
}
