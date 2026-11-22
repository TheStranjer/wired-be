//! Account utilities: aliases, moves, credentials and departure.
//!
//! `/api/pleroma/aliases`, `/api/pleroma/move_account` (WBE-137) plus
//! `/api/pleroma/change_password`, `/api/pleroma/change_email`,
//! `/api/pleroma/delete_account` and `/api/pleroma/disable_account`
//! (WBE-136), Pleroma's `UtilController` actions — and the Mastodon twin of
//! the move, `POST /api/v1/accounts/move` from [`mastodon_router`], the same
//! write and the same `Move`/`Update` federation keyed by the destination's
//! account id and answered with the caller's relationship entity.
//!
//! The reference is `list_aliases`, `add_alias`, `delete_alias`,
//! `move_account`, `change_password`, `change_email`, `delete_account` and
//! `disable_account` in `lib/pleroma/web/pleroma_api/controllers/
//! util_controller.ex`, with the state changes behind Pleroma's
//! `ActivityPub.move/3`
//! (`lib/pleroma/web/activity_pub/activity_pub.ex`) and
//! `FollowingRelationship.move_following/2`. The ticket shapes are
//! authoritative where Pleroma's differ.
//!
//! Every route requires a user token ([`AuthUser`]): `GET` asks the
//! `read:accounts` scope, everything that changes state the
//! `write:accounts` scope. The state lives in the `users` columns each
//! action owns — `also_known_as` (the alias list of `ActivityPub` actor
//! ids), `moved_to_ap_id` and `last_move_at` (the move and its cooldown),
//! `password_hash`, `email` with `email_confirmed`, and `is_active` with
//! `deleted_at` (the departure pair) — written through the model writers
//! in [`crate::models::user`] and [`crate::models::oauth_token`].
//!
//! The endpoints:
//!
//! - `GET /api/pleroma/aliases` — `{"aliases": [handle, …]}`: the stored
//!   actor ids rendered as `acct` handles (a mirrored actor by its
//!   full nickname, like Pleroma's `list_aliases`); an id this instance
//!   has no row for falls back to the raw URL.
//! - `PUT /api/pleroma/aliases` — body `alias` (an `acct` handle or an
//!   `ActivityPub` id), resolved through the federation get-or-fetch
//!   ([`get_or_fetch_by_acct`] / [`get_or_fetch_by_ap_id`]); a resolution
//!   failure is 404 `{"error":"Record not found"}`. The target's actor id
//!   is appended to the list unless it is already there; `{"status":
//!   "success"}`.
//! - `DELETE /api/pleroma/aliases` — body `alias`: drops the matching
//!   entries (the string as given, and the actor id of the row that
//!   handle names), then `{"status":"success"}`. Removing an alias that
//!   is not on the list changes nothing and still succeeds.
//! - `POST /api/pleroma/move_account` — `password` plus
//!   `target_account` (an `acct` handle), gated in Pleroma's order: the
//!   password first (a wrong one answers 200 `{"error":
//!   "Invalid password."}` — the shape Pleroma's `json(conn, %{error:
//!   msg})` renders), the target's resolution 404 `{"error":"Target
//!   account not found."}`, and the account move itself — Pleroma's
//!   `ActivityPub.move` refuses when the target has not listed the
//!   origin in `alsoKnownAs` — as 200 with Pleroma's message. The
//!   cooldown (Pleroma's once-per-30-days move limit, kept in
//!   `last_move_at`) is checked at the same 200-with-error shape. Once
//!   every gate passes the account is marked moved, its followers
//!   transfer, and the `after_account_moved` hook runs.
//! - `POST /api/pleroma/change_password` — `password`, `new_password`
//!   and `new_password_confirmation`: after the password gate, the
//!   new password's confirmation and length are checked at the same
//!   200-with-error shape (`"New password does not match confirmation."`
//!   and `"Password too short."` — the ticket's wording for Pleroma's
//!   `change_password` refusals). A pass stores the new Argon2id hash and
//!   revokes every one of the account's other OAuth tokens, keeping the
//!   credential the request authenticated with; `{"status":"success"}`.
//! - `POST /api/pleroma/change_email` — `password` and `email`: after the
//!   password gate the address must look like `x@y.z` (200 `"Email has
//!   invalid format."`) and belong to no other account (200 `"Email has
//!   already been taken"`, the ticket's spelling of Pleroma's changeset
//!   message). The write stores the address; on an instance that requires
//!   account confirmation
//!   ([`account_activation_required`](crate::config::Config::account_activation_required))
//!   the account becomes unconfirmed and a confirmation letter for the
//!   new address is queued; `{"status":"success"}`.
//! - `POST /api/pleroma/delete_account` — `password`: while
//!   [`allow_account_deletion`](crate::config::Config::allow_account_deletion)
//!   is off the endpoint answers 403 `{"error":"Account deletion is
//!   disabled"}` before any other gate. A pass marks the account inactive
//!   with `deleted_at` stamped, revokes all its tokens (the presenting
//!   credential included) and enqueues the
//!   [`users.delete`](crate::jobs::deletion::KIND_DELETE) job on the
//!   background queue, which tombstones the account's statuses, deletes
//!   its follows, favourites, bookmarks, notifications, lists and media
//!   rows, and blanks the personal data of the kept row (decision 7:
//!   other tables still reference the id); `{"status":"success"}`.
//! - `POST /api/pleroma/disable_account` — `password`: deactivates the
//!   account (`is_active = false`, no `deleted_at`, every datum kept) and
//!   revokes all its tokens; `{"status":"success"}`. Pleroma's
//!   `User.set_activation(user, false)`.
//!
//! The four utilities above all take the account's current `password`,
//! verified against the stored hash exactly like `move_account` does: no
//! hash on the row, or a mismatch, is the same 200 `"Invalid password."`,
//! and every state change runs only past that gate.
//!
//! The follower transfer mirrors Pleroma's `move_following`: every
//! *local* follower with an accepted relationship that still allows its
//! following list to move follows the target through the normal
//! [`follow::follow`] logic (so the follow is accepted immediately when
//! this instance owns an unlocked target, and a blocked pair is refused
//! and skipped), and unfollows the origin; the denormalised counters move
//! with those two calls. Remote followers' relationships are their own
//! instances' to rewrite, and stay.
//!
//! Federation of the `Move` activity and the move notifications: the
//! federating half has landed —
//! [`crate::services::federation_out::after_account_moved`] publishes the
//! `Move` to the origin's remote followers and an `Update` of the re-read
//! actor (which now carries its `movedTo`) through the hook
//! `move_account` runs. The move notifications attach to the same hook
//! later. The federated `Delete` of a deleted account's actor is the
//! `users.delete` sweep's own: it runs in
//! [`crate::services::federation_out::after_account_deleted`] while the
//! sweep still holds the relationship rows the fan-out reads. A disabled
//! account publishes nothing, the decision
//! [`crate::services::federation_out::after_account_disabled`] documents.
//!
//! [`get_or_fetch_by_acct`]: crate::federation::fetch_actor::get_or_fetch_by_acct
//! [`get_or_fetch_by_ap_id`]: crate::federation::fetch_actor::get_or_fetch_by_ap_id

use std::fmt;

use axum::{
    Json, Router,
    routing::{get, post},
};
use chrono::Duration;
use serde::Deserialize;
use serde_json::{Value, json};

mod account_state;
mod aliases;
mod credentials;
mod moves;
#[cfg(test)]
mod test_support;

use self::{
    account_state::{delete_account, disable_account},
    aliases::{add_alias, delete_alias, list_aliases},
    credentials::{change_email, change_password},
    moves::{move_account, move_by_id},
};
#[cfg(doc)]
use crate::{auth::AuthUser, entities::relationship::Relationship, models::follow};
use crate::{models::user::User, state::AppState, util::password::verify_password};

/// Whether `text` is given as an `ActivityPub` actor id rather than an
/// `acct` handle: a case-insensitive `http://` or `https://` prefix.
/// Shared with [`crate::api::accounts`]' `POST /api/v1/follows`, which
/// splits its `uri` parameter the same way.
pub(crate) fn looks_like_ap_id(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// 200 error body when the submitted password does not match the stored
/// hash (Pleroma's `confirm_current_password` wording, period included —
/// `move_account` renders it through `json(conn, %{error: msg})`, which
/// is why these failures are 200s, not 4xx).
const INVALID_PASSWORD: &str = "Invalid password.";
/// 200 error body when the new password of `change_password` differs
/// from its confirmation (Pleroma's `:password_confirmation` refusal).
const PASSWORD_MISMATCH: &str = "New password does not match confirmation.";
/// 200 error body when the new password of `change_password` is shorter
/// than [`min_password_length`](crate::config::Config::min_password_length)
/// (the ticket's wording for Pleroma's password-length changeset error).
const PASSWORD_TOO_SHORT: &str = "Password too short.";
/// 200 error body when the new address of `change_email` does not look
/// like `x@y.z` (Pleroma's `validate_format` error, rendered as
/// `"Email #{error}."`).
const EMAIL_INVALID_FORMAT: &str = "Email has invalid format.";
/// 200 error body when another account already carries the new address of
/// `change_email` (the ticket's spelling of Pleroma's `unique_constraint`
/// message, no period).
const EMAIL_TAKEN: &str = "Email has already been taken";
/// 403 error body of `delete_account` on an instance that switched
/// account deletion off (the ticket's wording, no period).
const DELETION_DISABLED: &str = "Account deletion is disabled";
/// Path a `change_email` confirmation letter links to, before the
/// account id and its confirmation token. The endpoint that consumes the
/// token is the later confirmation ticket's; the address is fixed here so
/// a letter already sent keeps pointing at it.
const CONFIRM_EMAIL_PATH: &str = "/api/pleroma/account/confirm_email";
/// 404 error body when the move target cannot be resolved (Pleroma's
/// `move_account` `:not_found` branch, period included).
const TARGET_NOT_FOUND: &str = "Target account not found.";
/// 200 error body when the target has not listed the moving account as
/// an alias (Pleroma's `ActivityPub.move` refusal, backticks and all).
const MOVE_ALIAS_REQUIRED: &str = "Target account must have the origin in `alsoKnownAs`";
/// 200 error body when the account moved within the cooldown window
/// (Pleroma's `move_account` rate limit wording).
const MOVE_COOLDOWN: &str = "You can't move your account more than once in 30 days";
/// How long an account must wait between moves (Pleroma's
/// once-per-30-days `move_account` cooldown).
const MOVE_COOLDOWN_WINDOW: Duration = Duration::days(30);

/// The `/api/pleroma` account-settings router, merged into
/// [`crate::api::pleroma`].
///
/// Do not call [`Router::fallback`] here (or on anything merged into this
/// tree): the app-wide JSON fallback in [`crate::app`] must answer the
/// unmatched paths.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/aliases",
            get(list_aliases).put(add_alias).delete(delete_alias),
        )
        .route("/move_account", post(move_account))
        .route("/change_password", post(change_password))
        .route("/change_email", post(change_email))
        .route("/delete_account", post(delete_account))
        .route("/disable_account", post(disable_account))
}

/// The `/api/v1/accounts/move` twin of the Pleroma `move_account` action.
///
/// Merged into [`crate::api::v1`]: the same gates and the same `Move`
/// federation, keyed by the destination's account id and answered with the
/// caller's [`Relationship`] entity.
///
/// Do not call [`Router::fallback`] here (or on anything merged into the
/// `/api/v1` tree): the app-wide JSON fallback in [`crate::app`] must answer
/// the unmatched paths.
pub fn mastodon_router() -> Router<AppState> {
    Router::new().route("/accounts/move", post(move_by_id))
}

/// The parameters of `POST /api/pleroma/delete_account` and
/// `POST /api/pleroma/disable_account`: the credential gate, alone.
#[derive(Deserialize)]
struct PasswordParams {
    /// The account's current password; secret material.
    password: String,
}

/// `Debug` redacts the password.
impl fmt::Debug for PasswordParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasswordParams")
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Whether `password` matches the account's stored hash. A row without
/// one (a bot-created account, a mirror) fails exactly like a wrong
/// password, as Pleroma's `confirm_current_password` does.
fn password_accepted(account: &User, password: &str) -> bool {
    account
        .password_hash
        .as_deref()
        .is_some_and(|hash| verify_password(password, hash))
}

/// The `{"status":"success"}` body all four endpoints answer success
/// with.
fn success_body() -> Json<Value> {
    Json(json!({"status": "success"}))
}

/// The 200 error body Pleroma's `json(conn, %{error: msg})` renders for
/// the refusals that are not status errors.
fn error_body(message: &str) -> Json<Value> {
    Json(json!({"error": message}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_and_error_bodies_have_the_documented_shapes() {
        assert_eq!(success_body().0, json!({"status": "success"}));
        assert_eq!(
            error_body(INVALID_PASSWORD).0,
            json!({"error": "Invalid password."})
        );
    }
}
