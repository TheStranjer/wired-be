//! Moving the account: the Pleroma and Mastodon move endpoints, the
//! cooldown and the follower transfer.

use std::fmt;

use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;

use super::{
    INVALID_PASSWORD, MOVE_ALIAS_REQUIRED, MOVE_COOLDOWN, MOVE_COOLDOWN_WINDOW, TARGET_NOT_FOUND,
    aliases::resolve_alias, error_body, password_accepted, success_body,
};
use crate::{
    api::accounts::after_follow_changed,
    auth::AuthUser,
    entities::relationship::Relationship,
    error::{AppError, AppResult},
    extract::ApiBody,
    models::{
        follow,
        user::{self, User},
    },
    oauth::scopes::WRITE_ACCOUNTS,
    services::federation_out,
    state::AppState,
};

/// The parameters of `POST /api/v1/accounts/move`: the same move Pleroma's
/// `move_account` performs, keyed by the destination's account id (Mastodon's
/// spelling) instead of its `acct` handle.
#[derive(Deserialize)]
pub(super) struct MoveByIdParams {
    /// The account's current password; secret material.
    password: String,
    /// The id of the account to move to.
    to_account: String,
}

/// `Debug` redacts the password.
impl fmt::Debug for MoveByIdParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MoveByIdParams")
            .field("password", &"<redacted>")
            .field("to_account", &self.to_account)
            .finish()
    }
}

/// The parameters of `POST /api/pleroma/move_account`.
#[derive(Deserialize)]
pub(super) struct MoveParams {
    /// The account's current password; secret material.
    password: String,
    /// The `acct` handle to move to.
    target_account: String,
}

/// `Debug` redacts the password.
impl fmt::Debug for MoveParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MoveParams")
            .field("password", &"<redacted>")
            .field("target_account", &self.target_account)
            .finish()
    }
}

/// `POST /api/pleroma/move_account` — move the calling account to
/// `target_account`. The gates run in Pleroma's order (password, target
/// resolution, alias precondition, cooldown), all before any state
/// changes; the first one that refuses answers, and only a fully gated
/// move marks the account moved and transfers its followers.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::NotFoundMessage`] (404
/// "Target account not found.") when the target resolves to no account.
/// The password, alias and cooldown refusals are 200 error bodies, as
/// Pleroma renders them. Any database failure arrives through `?` as
/// [`AppError::Database`].
pub(super) async fn move_account(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<MoveParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    if !password_accepted(&auth.0, &params.password) {
        return Ok(error_body(INVALID_PASSWORD));
    }
    let target = resolve_move_target(&state, &params.target_account).await?;
    if !target_lists_origin(&target, &auth.0) {
        return Ok(error_body(MOVE_ALIAS_REQUIRED));
    }
    if moved_within_cooldown(auth.0.last_move_at) {
        return Ok(error_body(MOVE_COOLDOWN));
    }
    user::mark_moved(&state.db, auth.0.id, &target.ap_id).await?;
    transfer_followers(&state, &auth.0, &target).await?;
    after_account_moved(&state, &auth.0, &target).await;
    Ok(success_body())
}

/// `POST /api/v1/accounts/move` — move the calling account onto the account
/// whose id [`MoveByIdParams::to_account`] names, answering the caller's
/// [`Relationship`] to it (Mastodon's shape for this write).
///
/// The gates are [`move_account`]'s in Pleroma's order — password, then the
/// alias precondition, then the cooldown — and the same writes stand behind
/// the success: the account is marked moved, its transferable followers
/// follow the destination, and the
/// [`crate::services::federation_out::after_account_moved`] hook publishes
/// the `Move` and the profile `Update` carrying the new `movedTo`. Where the
/// two routes differ is the answer of a refusal and the resolution of the
/// destination: this one keys it by account id, and its refusals are proper
/// 4xx statuses rendering the same wordings — a wrong password and the two
/// gate refusals are 422s, a malformed or unknown id the same 404 "Target
/// account not found." the Pleroma route answers with. Pleroma's 200-body
/// refusals have no place in a Mastodon-shaped endpoint; the ticket's
/// wordings are kept verbatim.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`] and
/// the `write:accounts` check; [`AppError::Unprocessable`] for a wrong
/// password, a destination that has not listed the origin as an alias, or a
/// move inside the cooldown window; [`AppError::NotFoundMessage`] (404) for
/// a destination that is malformed or names no stored account. Any database
/// failure arrives through `?` as [`AppError::Database`].
pub(super) async fn move_by_id(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<MoveByIdParams>,
) -> AppResult<Json<Relationship>> {
    auth.require(WRITE_ACCOUNTS)?;
    if !password_accepted(&auth.0, &params.password) {
        return Err(AppError::Unprocessable(INVALID_PASSWORD.to_owned()));
    }
    let target = resolve_move_target_by_id(&state, &params.to_account).await?;
    if !target_lists_origin(&target, &auth.0) {
        return Err(AppError::Unprocessable(MOVE_ALIAS_REQUIRED.to_owned()));
    }
    if moved_within_cooldown(auth.0.last_move_at) {
        return Err(AppError::Unprocessable(MOVE_COOLDOWN.to_owned()));
    }
    user::mark_moved(&state.db, auth.0.id, &target.ap_id).await?;
    transfer_followers(&state, &auth.0, &target).await?;
    after_account_moved(&state, &auth.0, &target).await;
    Ok(Json(
        Relationship::load_one(&state.db, auth.0.id, target.id).await?,
    ))
}

/// Resolves the destination of `POST /api/v1/accounts/move`: an account id
/// (decision 4's text) naming a stored account — local or a mirror this
/// instance already holds. A malformed or unknown id is the same
/// `"Target account not found."` 404 the Pleroma route answers for its
/// handle-shaped parameter.
async fn resolve_move_target_by_id(state: &AppState, id: &str) -> AppResult<User> {
    let Some(parsed) = crate::util::id::parse_id(id) else {
        return Err(AppError::NotFoundMessage(TARGET_NOT_FOUND.to_owned()));
    };
    user::get_by_id(&state.db, parsed)
        .await?
        .ok_or_else(|| AppError::NotFoundMessage(TARGET_NOT_FOUND.to_owned()))
}

/// Resolves a move target like [`resolve_alias`], replacing its generic
/// 404 with the message the `move_account` endpoint owes ("Target
/// account not found.").
pub(super) async fn resolve_move_target(state: &AppState, acct: &str) -> AppResult<User> {
    resolve_alias(state, acct).await.map_err(|err| {
        debug!(%err, acct, "move target could not be resolved");
        AppError::NotFoundMessage(TARGET_NOT_FOUND.to_owned())
    })
}

/// The `ActivityPub.move` precondition: the target must have listed the
/// moving account's own actor id among its aliases.
fn target_lists_origin(target: &User, origin: &User) -> bool {
    target.also_known_as.contains(&origin.ap_id)
}

/// Whether `last_move_at` falls inside the [`MOVE_COOLDOWN_WINDOW`]. A
/// stamp in the future (clock skew) counts as recent rather than
/// letting the account move twice around it.
fn moved_within_cooldown(last_move_at: Option<DateTime<Utc>>) -> bool {
    last_move_at.is_some_and(|moved| Utc::now().signed_duration_since(moved) < MOVE_COOLDOWN_WINDOW)
}

/// Moves `origin`'s followers onto `target`: every candidate from
/// [`follow::move_followers_of`] (local, accepted, `allow_following_move`)
/// follows the target through [`follow::follow`] — accepted on the spot
/// when this instance owns an unlocked target, refused-and-skipped when
/// the pair is blocked — and then unfollows the origin through
/// [`follow::unfollow`]. Both calls move the denormalised counters; the
/// follower's own `following_count` ends unchanged (up with the target,
/// down with the origin). A follower whose re-follow failed keeps its
/// original relationship untouched: nothing about the move is applied
/// half-way. Each write then runs [`after_follow_changed`], so a remote
/// target is sent the `Follow` (Pleroma's `CommonAPI.follow`), a locked
/// local one gets its request notification, and the origin's side of the
/// unfollow stays consistent with the interactive unfollow.
///
/// # Errors
///
/// Any database failure from the candidate query or the unfollow arrives
/// as [`AppError::Database`]; per-follower [`AppError::Unprocessable`]
/// refusals (a blocked pair, Pleroma's `deny_follow_blocked`) only log
/// and skip.
async fn transfer_followers(state: &AppState, origin: &User, target: &User) -> AppResult<()> {
    for follower in follow::move_followers_of(&state.db, origin.id).await? {
        let created =
            match follow::follow(&state.db, &state.config.base_url, &follower, target).await {
                Ok(row) => row,
                Err(err) => {
                    debug!(%err, follower = %follower.nickname, "skipped during follower transfer");
                    continue;
                }
            };
        after_follow_changed(state, &follower, target, Some(&created), true).await;
        if follow::unfollow(&state.db, &follower, origin).await? {
            after_follow_changed(state, &follower, origin, None, false).await;
        }
    }
    Ok(())
}

/// The hook the move runs once the account is marked moved and its
/// followers have transferred: announces the move through
/// [`federation_out::after_account_moved`] — Pleroma's `ActivityPub.move`
/// fan-out to the origin's remote followers, plus the profile `Update`
/// that carries the new `movedTo`. The move notifications attach here
/// with their own ticket; the hook never fails the request (decision 11
/// queues the delivery, and the federation hook swallows its own enqueue
/// errors).
async fn after_account_moved(state: &AppState, old: &User, new: &User) {
    federation_out::after_account_moved(state, old, new).await;
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};

    use super::super::{
        looks_like_ap_id,
        test_support::{ALIAS, aliases, template_user},
    };
    use super::*;

    #[test]
    fn ap_ids_are_told_apart_from_handles() {
        for url in [
            "https://remote.example/users/bob",
            "http://remote.example/users/bob",
            "HTTPS://REMOTE.EXAMPLE/users/bob",
        ] {
            assert!(looks_like_ap_id(url), "`{url}` is an actor id");
        }
        for handle in ["bob@remote.example", "alice", "ftp://x.example/a"] {
            assert!(!looks_like_ap_id(handle), "`{handle}` is not an actor id");
        }
    }

    #[test]
    fn the_move_precondition_reads_the_target_s_alias_list() {
        let mut origin = template_user("alice");
        origin.ap_id = ALIAS.to_owned();
        let mut target = template_user("bob@remote.example");

        assert!(!target_lists_origin(&target, &origin), "no aliases yet");
        target.also_known_as = vec!["https://elsewhere.example/users/x".to_owned()];
        assert!(
            !target_lists_origin(&target, &origin),
            "an alias of someone else is not the origin"
        );
        target.also_known_as = aliases(&[]);
        assert!(
            target_lists_origin(&target, &origin),
            "the origin's own actor id satisfies the precondition"
        );
    }

    #[test]
    fn cooldown_is_thirty_days_and_clock_skew_counts_recent() {
        assert!(
            !moved_within_cooldown(None),
            "an account that never moved may move"
        );
        assert!(
            moved_within_cooldown(Some(Utc::now() - Duration::days(29))),
            "29 days ago is inside the window"
        );
        assert!(
            !moved_within_cooldown(Some(Utc::now() - Duration::days(31))),
            "31 days ago is outside it"
        );
        assert!(
            moved_within_cooldown(Some(Utc::now() + Duration::days(1))),
            "a future stamp is treated as recent, not as free rein"
        );
    }

    #[test]
    fn move_params_debug_redacts_the_password() {
        let params = MoveParams {
            password: "hunter2".to_owned(),
            target_account: "bob@remote.example".to_owned(),
        };
        let shown = format!("{params:?}");
        assert!(
            !shown.contains("hunter2") && shown.contains("bob@remote.example"),
            "the Debug hides the credential and keeps the handle: {shown}"
        );
    }
}
