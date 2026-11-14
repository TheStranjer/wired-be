//! Account utilities: aliases, moves, credentials and departure.
//!
//! `/api/pleroma/aliases`, `/api/pleroma/move_account` (WBE-137) plus
//! `/api/pleroma/change_password`, `/api/pleroma/change_email`,
//! `/api/pleroma/delete_account` and `/api/pleroma/disable_account`
//! (WBE-136), Pleroma's `UtilController` actions.
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
    extract::State,
    routing::{get, post},
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::debug;

use crate::{
    api::instance::sha256_hex,
    auth::AuthUser,
    entities::account::full_nickname,
    error::{AppError, AppResult},
    extract::ApiBody,
    federation::fetch_actor::{get_or_fetch_by_acct, get_or_fetch_by_ap_id},
    jobs::{QUEUE_BACKGROUND, deletion},
    mailer::{self, templates},
    models::{
        follow,
        job::{NewJob, enqueue},
        oauth_token,
        user::{self, User, get_by_ap_id, get_by_email},
    },
    oauth::scopes::{READ_ACCOUNTS, WRITE_ACCOUNTS},
    services::federation_out,
    state::AppState,
    util::password::{hash_password, verify_password},
};

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

/// The `{"aliases": […]}` body of [`list_aliases`].
#[derive(Debug, Serialize)]
struct AliasesBody {
    /// The account's aliases as `acct` handles (or raw actor URLs when
    /// this instance has no row for the id).
    aliases: Vec<String>,
}

/// The parameters of `PUT`/`DELETE /api/pleroma/aliases`: the alias to
/// add or drop, as an `acct` handle or an `ActivityPub` actor id.
#[derive(Debug, Deserialize)]
struct AliasParams {
    /// The alias the client names.
    alias: String,
}

/// The parameters of `POST /api/pleroma/move_account`.
#[derive(Deserialize)]
struct MoveParams {
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

/// The parameters of `POST /api/pleroma/change_password`.
#[derive(Deserialize)]
struct ChangePasswordParams {
    /// The account's current password; secret material.
    password: String,
    /// The password to store; secret material.
    new_password: String,
    /// The repeated [`new_password`](Self::new_password); secret material.
    new_password_confirmation: String,
}

/// All three fields are secret material and render as `<redacted>`.
impl fmt::Debug for ChangePasswordParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChangePasswordParams")
            .field("password", &"<redacted>")
            .field("new_password", &"<redacted>")
            .field("new_password_confirmation", &"<redacted>")
            .finish()
    }
}

/// The parameters of `POST /api/pleroma/change_email`.
#[derive(Deserialize)]
struct ChangeEmailParams {
    /// The account's current password; secret material.
    password: String,
    /// The address to store.
    email: String,
}

/// `Debug` redacts the password.
impl fmt::Debug for ChangeEmailParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChangeEmailParams")
            .field("password", &"<redacted>")
            .field("email", &self.email)
            .finish()
    }
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

/// `GET /api/pleroma/aliases` — the account's alias list as handles.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `read:accounts` check. Any database failure arrives through
/// `?` as [`AppError::Database`].
async fn list_aliases(
    State(state): State<AppState>,
    auth: AuthUser,
) -> AppResult<Json<AliasesBody>> {
    auth.require(READ_ACCOUNTS)?;
    let aliases = alias_handles(&state, &auth.0).await?;
    Ok(Json(AliasesBody { aliases }))
}

/// The stored alias actor ids rendered one by one: the full `acct`
/// nickname of the mirrored actor when this instance has a row for the
/// id (Pleroma's `list_aliases` maps through `User.full_nickname/1`),
/// the raw URL when it has none.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn alias_handles(state: &AppState, account: &User) -> AppResult<Vec<String>> {
    let mut handles = Vec::with_capacity(account.also_known_as.len());
    for ap_id in &account.also_known_as {
        let handle = get_by_ap_id(&state.db, ap_id).await?.map_or_else(
            || ap_id.clone(),
            |found| full_nickname(&found, &state.config),
        );
        handles.push(handle);
    }
    Ok(handles)
}

/// `PUT /api/pleroma/aliases` — append an alias. The target is resolved
/// through the federation get-or-fetch (see [`resolve_alias`]); its
/// actor id joins the list unless it is already there, so adding twice
/// is one entry. Answers `{"status":"success"}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check; [`AppError::NotFound`] (the generic
/// "Record not found" body) when the alias resolves to no account. Any
/// database failure arrives through `?` as [`AppError::Database`].
async fn add_alias(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<AliasParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    let target = resolve_alias(&state, &params.alias).await?;
    let mut aliases = auth.0.also_known_as.clone();
    if !aliases.contains(&target.ap_id) {
        aliases.push(target.ap_id.clone());
        user::set_also_known_as(&state.db, auth.0.id, &aliases).await?;
    }
    Ok(success_body())
}

/// `DELETE /api/pleroma/aliases` — drop an alias. Both spellings a
/// client may echo back are removed: the string exactly as given (an
/// entry [`list_aliases`] rendered as its raw URL falls back to that)
/// and, when the input names a row this instance knows, that row's
/// actor id. A list without a matching entry is written unchanged; the
/// answer is `{"status":"success"}` either way.
///
/// Unlike `PUT` this never touches the network: removal needs no fresh
/// view of the target, only its stored id.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check. Any database failure arrives through
/// `?` as [`AppError::Database`].
async fn delete_alias(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<AliasParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    let stored = &auth.0.also_known_as;
    let kept = dropped_aliases(&state, &params.alias, stored).await?;
    if kept.len() != stored.len() {
        user::set_also_known_as(&state.db, auth.0.id, &kept).await?;
    }
    Ok(success_body())
}

/// The alias list with `input` removed, as [`delete_alias`] writes it:
/// entries equal to the input verbatim, or to the actor id of the row
/// the input names (the mirrored-alias case, where `GET` showed a handle
/// rather than the raw id). The `Vec` is rebuilt (order-preserving) so a
/// client that added the same alias under both spellings is fully
/// cleared.
///
/// # Errors
///
/// Any database failure arrives through `?` as [`AppError::Database`].
async fn dropped_aliases(
    state: &AppState,
    input: &str,
    stored: &[String],
) -> AppResult<Vec<String>> {
    let known_id = user::get_by_acct(&state.db, &state.config, input)
        .await?
        .map(|found| found.ap_id);
    Ok(without_alias(stored, input, known_id.as_deref()))
}

/// The list-editing half of [`dropped_aliases`], pure: keep every entry
/// that is neither the input verbatim nor the resolved actor id
/// (`known_id`, `None` when the input names no row).
fn without_alias(stored: &[String], input: &str, known_id: Option<&str>) -> Vec<String> {
    stored
        .iter()
        .filter(|alias| **alias != input && Some(alias.as_str()) != known_id)
        .cloned()
        .collect()
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
async fn move_account(
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

/// Whether `password` matches the account's stored hash. A row without
/// one (a bot-created account, a mirror) fails exactly like a wrong
/// password, as Pleroma's `confirm_current_password` does.
fn password_accepted(account: &User, password: &str) -> bool {
    account
        .password_hash
        .as_deref()
        .is_some_and(|hash| verify_password(password, hash))
}

/// Resolves an alias string to the account behind it: an `http(s)` URL
/// goes through [`get_or_fetch_by_ap_id`], anything else through
/// [`get_or_fetch_by_acct`] (which answers from the stored nickname when
/// there is one, and only then reaches for `WebFinger`). A failure to
/// resolve is reported as the plain 404 the ticket fixes; the fetcher's
/// own reason is logged, never sent.
///
/// [`get_or_fetch_by_ap_id`]: crate::federation::fetch_actor::get_or_fetch_by_ap_id
/// [`get_or_fetch_by_acct`]: crate::federation::fetch_actor::get_or_fetch_by_acct
async fn resolve_alias(state: &AppState, alias: &str) -> AppResult<User> {
    let fetched = if looks_like_ap_id(alias) {
        get_or_fetch_by_ap_id(state, alias).await
    } else {
        get_or_fetch_by_acct(state, alias).await
    };
    fetched.map_err(|err| {
        debug!(%err, alias, "alias target could not be resolved");
        AppError::NotFound
    })
}

/// Resolves a move target like [`resolve_alias`], replacing its generic
/// 404 with the message the `move_account` endpoint owes ("Target
/// account not found.").
async fn resolve_move_target(state: &AppState, acct: &str) -> AppResult<User> {
    resolve_alias(state, acct).await.map_err(|err| {
        debug!(%err, acct, "move target could not be resolved");
        AppError::NotFoundMessage(TARGET_NOT_FOUND.to_owned())
    })
}

/// Whether `text` is given as an `ActivityPub` actor id rather than an
/// `acct` handle: a case-insensitive `http://` or `https://` prefix.
/// Shared with [`crate::api::accounts`]' `POST /api/v1/follows`, which
/// splits its `uri` parameter the same way.
pub(crate) fn looks_like_ap_id(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
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
/// half-way.
///
/// # Errors
///
/// Any database failure from the candidate query or the unfollow arrives
/// as [`AppError::Database`]; per-follower [`AppError::Unprocessable`]
/// refusals (a blocked pair, Pleroma's `deny_follow_blocked`) only log
/// and skip.
async fn transfer_followers(state: &AppState, origin: &User, target: &User) -> AppResult<()> {
    for follower in follow::move_followers_of(&state.db, origin.id).await? {
        if let Err(err) = follow::follow(&state.db, &state.config.base_url, &follower, target).await
        {
            debug!(%err, follower = %follower.nickname, "skipped during follower transfer");
            continue;
        }
        follow::unfollow(&state.db, &follower, origin)
            .await
            .map(drop)?;
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

/// `POST /api/pleroma/change_password` — set a new password. The gates
/// run in order — the current password, then the confirmation, then the
/// length; every refusal is a 200 error body, as Pleroma renders them —
/// and a pass stores the new Argon2id hash and revokes every one of the
/// account's *other* OAuth tokens in one transaction. The credential the
/// request authenticated with keeps working; the password it was minted
/// for no longer exists, so every other client must log in again.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check. A wrong password, a mismatching
/// confirmation or a too-short new one answer with the 200 error bodies.
/// A hashing failure arrives through `?` as [`AppError::Internal`], any
/// database failure as [`AppError::Database`].
async fn change_password(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<ChangePasswordParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    if !password_accepted(&auth.0, &params.password) {
        return Ok(error_body(INVALID_PASSWORD));
    }
    if params.new_password != params.new_password_confirmation {
        return Ok(error_body(PASSWORD_MISMATCH));
    }
    if params.new_password.chars().count() < state.config.min_password_length {
        return Ok(error_body(PASSWORD_TOO_SHORT));
    }
    let hash = hash_password(&params.new_password)?;
    let revoked = write_password_and_revoke(&state, &auth, &hash).await?;
    debug!(account = %auth.0.nickname, revoked, "password changed: other tokens revoked");
    Ok(success_body())
}

/// The `change_password` write: the new hash and the revocation of every
/// token but the request's own, as one transaction — half of it is never
/// a state an account is left in.
async fn write_password_and_revoke(
    state: &AppState,
    auth: &AuthUser,
    hash: &str,
) -> AppResult<u64> {
    let mut tx = state.db.begin().await?;
    user::set_password(&mut *tx, auth.0.id, hash).await?;
    let revoked = oauth_token::delete_all_for_user_except(&mut *tx, auth.0.id, auth.1.id).await?;
    tx.commit().await?;
    Ok(revoked)
}

/// `POST /api/pleroma/change_email` — set a new address. After the
/// password gate the trimmed address must look like `x@y.z` and belong
/// to no other account — both refusals are the endpoint's 200 error
/// bodies. The write stores the address; when the instance requires
/// email confirmation the account becomes unconfirmed and a confirmation
/// letter for the new address is queued (dropped silently while no SMTP
/// relay is configured, see [`crate::mailer`]).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check. The password, format and uniqueness
/// refusals are 200 error bodies. Any database failure arrives through
/// `?` as [`AppError::Database`].
async fn change_email(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<ChangeEmailParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    if !password_accepted(&auth.0, &params.password) {
        return Ok(error_body(INVALID_PASSWORD));
    }
    let email = params.email.trim();
    if !user::register::is_plausible_email(email) {
        return Ok(error_body(EMAIL_INVALID_FORMAT));
    }
    if email_is_taken(&state, &auth.0, email).await? {
        return Ok(error_body(EMAIL_TAKEN));
    }
    let confirmed = !state.config.account_activation_required.get();
    user::set_email(&state.db, auth.0.id, email, confirmed).await?;
    if !confirmed {
        send_confirmation_mail(&state, &auth.0, email).await?;
    }
    debug!(account = %auth.0.nickname, confirmed, "email changed");
    Ok(success_body())
}

/// Whether `email` (already format-checked) is carried by an account
/// other than `account`: the citext column folds case, so "taken" means
/// taken in any spelling, while the account's own address is never
/// somebody else's taken.
async fn email_is_taken(state: &AppState, account: &User, email: &str) -> AppResult<bool> {
    Ok(get_by_email(&state.db, email)
        .await?
        .is_some_and(|holder| holder.id != account.id))
}

/// Queues the letter [`change_email`] sends a confirmation-requiring
/// instance: the registration-confirmation template addressed to the new
/// email, linking to [`CONFIRM_EMAIL_PATH`] with the account id and its
/// [`confirmation_token`].
async fn send_confirmation_mail(state: &AppState, account: &User, email: &str) -> AppResult<()> {
    let link = format!(
        "{}/{}/{}",
        CONFIRM_EMAIL_PATH,
        account.id,
        confirmation_token(account, email)
    );
    mailer::deliver_later(
        state,
        templates::registration_confirmation(&state.config, email, &link),
    )
    .await
}

/// The email-confirmation token for `email`: the lowercase hex `SHA-256`
/// of the account id, the address and the account's creation time. It is
/// derived rather than stored — the confirmation ticket that serves the
/// link can re-derive it from the row it reads, and because the address
/// is part of the digest, changing the email again invalidates the
/// letters already sent.
#[must_use]
fn confirmation_token(account: &User, email: &str) -> String {
    sha256_hex(&format!(
        "{}|{email}|{}",
        account.id,
        account.inserted_at.to_rfc3339()
    ))
}

/// `POST /api/pleroma/delete_account` — start deleting the account. The
/// config gate comes first: on an instance that switched deletion off
/// the endpoint answers 403 `{"error":"Account deletion is disabled"}`
/// whatever else the request said. Past it, the password gate, then the
/// start: the account is deactivated with `deleted_at` stamped and all
/// its tokens — the presenting one included — revoked in one transaction,
/// the [`users.delete`](deletion::KIND_DELETE) sweep is queued on
/// [`QUEUE_BACKGROUND`]. The `Delete` announcement to the peers is the
/// sweep's own first act (see
/// [`federation_out::after_account_deleted`](crate::services::federation_out::after_account_deleted)),
/// not the endpoint's: the announcement reads the relationship rows the
/// sweep is about to clear, and a client that gets this 200 must not wait
/// on either.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`],
/// the `write:accounts` check and the config gate. A wrong password is
/// the 200 error body. Any database failure arrives through `?` as
/// [`AppError::Database`].
async fn delete_account(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<PasswordParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    if !state.config.allow_account_deletion.get() {
        return Err(AppError::Forbidden(DELETION_DISABLED.to_owned()));
    }
    if !password_accepted(&auth.0, &params.password) {
        return Ok(error_body(INVALID_PASSWORD));
    }
    let revoked = mark_and_revoke_all(&state, &auth.0).await?;
    let queued = enqueue(
        &state.db,
        NewJob {
            queue: QUEUE_BACKGROUND,
            kind: deletion::KIND_DELETE,
            args: deletion::job_args(auth.0.id),
            run_at: None,
            max_attempts: None,
            unique_key: None,
        },
    )
    .await?;
    debug!(
        account = %auth.0.nickname,
        revoked,
        queued = queued.is_some(),
        "account deletion started"
    );
    Ok(success_body())
}

/// The `delete_account` write: inactive with `deleted_at` stamped, and
/// every token gone, as one transaction — a half-started deletion must
/// not leave the account active or still signed in.
async fn mark_and_revoke_all(state: &AppState, account: &User) -> AppResult<u64> {
    let mut tx = state.db.begin().await?;
    user::mark_deleted(&mut *tx, account.id).await?;
    let revoked = oauth_token::delete_all_for_user(&mut *tx, account.id).await?;
    tx.commit().await?;
    Ok(revoked)
}

/// `POST /api/pleroma/disable_account` — deactivate the account and keep
/// its data (Pleroma's `User.set_activation(user, false)`): the same
/// write as [`delete_account`]'s mark without the `deleted_at` stamp and
/// without the sweep, and its tokens are revoked whole because a
/// disabled account must not still be acting.
///
/// The account publishes nothing: a deactivation leaves the actor document
/// resolvable so the posts and follows peers already hold keep rendering,
/// and that decision lands in
/// [`federation_out::after_account_disabled`](crate::services::federation_out::after_account_disabled)
/// — the hook runs after a successful write and sends nothing, in this and
/// every configuration, until a ticket says otherwise.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AuthUser`]
/// and the `write:accounts` check. A wrong password is the 200 error
/// body. Any database failure arrives through `?` as [`AppError::Database`].
async fn disable_account(
    State(state): State<AppState>,
    auth: AuthUser,
    ApiBody(params): ApiBody<PasswordParams>,
) -> AppResult<Json<Value>> {
    auth.require(WRITE_ACCOUNTS)?;
    if !password_accepted(&auth.0, &params.password) {
        return Ok(error_body(INVALID_PASSWORD));
    }
    let revoked = deactivate_and_revoke_all(&state, &auth.0).await?;
    federation_out::after_account_disabled(&state, &auth.0);
    debug!(account = %auth.0.nickname, revoked, "account disabled");
    Ok(success_body())
}

/// The `disable_account` write: `is_active = false` and every token
/// gone, as one transaction.
async fn deactivate_and_revoke_all(state: &AppState, account: &User) -> AppResult<u64> {
    let mut tx = state.db.begin().await?;
    user::set_activation(&mut *tx, account.id, false).await?;
    let revoked = oauth_token::delete_all_for_user(&mut *tx, account.id).await?;
    tx.commit().await?;
    Ok(revoked)
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
    use uuid::Uuid;

    /// An account id the fixtures hand to the pure helpers.
    const ALIAS: &str = "https://old.example/users/alice";

    /// A [`User`]-shaped alias list for the pure list-editing assertions.
    fn aliases(extra: &[&str]) -> Vec<String> {
        let mut list = vec![ALIAS.to_owned()];
        list.extend(extra.iter().map(|alias| (*alias).to_owned()));
        list
    }

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

    /// A bare [`User`] with the two fields the pure helpers read and the
    /// rest at the constructor defaults (the shape of a fresh row).
    fn template_user(nickname: &str) -> User {
        User {
            id: Uuid::now_v7(),
            nickname: nickname.to_owned(),
            ap_id: format!("https://test.example/users/{nickname}"),
            local: user::Flag::new(true),
            domain: None,
            actor_type: "Person".to_owned(),
            email: None,
            password_hash: None,
            name: String::new(),
            bio: String::new(),
            raw_bio: String::new(),
            avatar_url: None,
            banner_url: None,
            profile_url: None,
            locked: user::Flag::new(false),
            bot: user::Flag::new(false),
            discoverable: user::Flag::new(false),
            fields: json!([]),
            raw_fields: json!([]),
            inbox: None,
            shared_inbox: None,
            outbox: None,
            followers_url: None,
            following_url: None,
            featured_url: None,
            public_key: None,
            private_key: None,
            is_admin: user::Flag::new(false),
            is_moderator: user::Flag::new(false),
            is_active: user::Flag::new(true),
            hide_followers: user::Flag::new(false),
            hide_follows: user::Flag::new(false),
            hide_followers_count: user::Flag::new(false),
            hide_follows_count: user::Flag::new(false),
            hide_favorites: user::Flag::new(true),
            allow_following_move: user::Flag::new(true),
            skip_thread_containment: user::Flag::new(false),
            accepts_chat_messages: user::Flag::new(true),
            show_role: user::Flag::new(true),
            no_rich_text: user::Flag::new(false),
            default_scope: "public".to_owned(),
            sensitive_default: user::Flag::new(false),
            language: None,
            tags: Vec::new(),
            settings_store: json!({}),
            notification_settings: json!({}),
            background_image_url: None,
            birthday: None,
            show_birthday: user::Flag::new(false),
            also_known_as: Vec::new(),
            moved_to_ap_id: None,
            last_move_at: None,
            email_confirmed: user::Flag::new(true),
            approval_pending: user::Flag::new(false),
            registration_reason: None,
            confirmation_token: None,
            confirmation_sent_at: None,
            last_active_at: None,
            followers_count: 0,
            following_count: 0,
            statuses_count: 0,
            last_status_at: None,
            last_fetched_at: None,
            inserted_at: Utc::now(),
            updated_at: Utc::now(),
            mfa_settings: user::mfa::MfaSettings::default_value(),
            preferred_frontend: None,
            is_suggested: user::Flag::new(false),
            password_reset_pending: user::Flag::new(false),
            email_notifications: user::Flag::new(true),
            deleted_at: None,
        }
    }

    /// The pure removal filter behind [`dropped_aliases`]: both spellings
    /// of the named alias go, everything else stays in order.
    #[test]
    fn removal_drops_the_verbatim_input_and_the_resolved_id() {
        let bob = "https://test.example/users/bob";
        let stored = aliases(&[bob, "https://x.example/users/y"]);

        let kept = without_alias(&stored, bob, Some(bob));
        assert_eq!(
            kept,
            vec![ALIAS.to_owned(), "https://x.example/users/y".to_owned()],
            "the verbatim spelling drops the entry with the same id"
        );

        let by_handle = without_alias(&stored, "bob@remote.example", Some(bob));
        assert_eq!(
            by_handle,
            vec![ALIAS.to_owned(), "https://x.example/users/y".to_owned()],
            "a handle spelling drops the row's actor id"
        );

        let untouched = without_alias(&stored, "ghost@nowhere", None);
        assert_eq!(
            untouched, stored,
            "an input nothing matches keeps every entry"
        );
    }

    #[test]
    fn success_and_error_bodies_have_the_documented_shapes() {
        assert_eq!(success_body().0, json!({"status": "success"}));
        assert_eq!(
            error_body(INVALID_PASSWORD).0,
            json!({"error": "Invalid password."})
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

    #[test]
    fn change_password_params_debug_redacts_all_three_secrets() {
        let params: ChangePasswordParams = serde_json::from_value(json!({
            "password": "hunter2",
            "new_password": "s3cret-new",
            "new_password_confirmation": "s3cret-new",
        }))
        .unwrap();
        let shown = format!("{params:?}");
        assert!(
            !shown.contains("hunter2") && !shown.contains("s3cret-new"),
            "no password material shows in Debug: {shown}"
        );
        assert!(
            shown.contains(r#"new_password_confirmation: "<redacted>""#),
            "the markers name the fields: {shown}"
        );
    }

    #[test]
    fn change_email_and_password_params_debug_redact_the_password() {
        let email: ChangeEmailParams = serde_json::from_value(json!({
            "password": "hunter2",
            "email": "new@example.org",
        }))
        .unwrap();
        let shown = format!("{email:?}");
        assert!(
            !shown.contains("hunter2") && shown.contains("new@example.org"),
            "the credential hides, the address shows: {shown}"
        );
        let bare: PasswordParams = serde_json::from_value(json!({"password": "hunter2"})).unwrap();
        let bare_shown = format!("{bare:?}");
        assert!(
            !bare_shown.contains("hunter2"),
            "the lone-password params hide their secret: {bare_shown}"
        );
    }

    #[test]
    fn the_confirmation_token_is_stable_and_address_bound() {
        let account = template_user("alice");
        let first = confirmation_token(&account, "new@example.org");
        assert_eq!(first.len(), 64, "a SHA-256 digest in lowercase hex");
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lowercase hex, nothing else: {first}"
        );
        assert_eq!(
            first,
            confirmation_token(&account, "new@example.org"),
            "the same account and address derive the same token"
        );
        assert_ne!(
            first,
            confirmation_token(&account, "other@example.org"),
            "a different address invalidates the letter already sent"
        );
        let mut renamed = template_user("alice");
        renamed.inserted_at = account.inserted_at + Duration::seconds(1);
        assert_ne!(
            first,
            confirmation_token(&renamed, "new@example.org"),
            "the creation time salts the digest"
        );
    }
}
