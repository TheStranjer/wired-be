//! The admin user endpoints under `/api/v1/pleroma/admin/users`.
//!
//! WBE-195 adds the account reads, WBE-198 the credential and security
//! writes, WBE-197 the tags, staff groups and suggestion pool, and WBE-196
//! the account lifecycle: create, delete, activate, deactivate, toggle and
//! approve.
//!
//! The reference is Pleroma's `AdminAPI.UserController`
//! (`lib/pleroma/web/admin_api/controllers/user_controller.ex`), `index`
//! and `show`, and its `AdminAPIController`
//! (`lib/pleroma/web/admin_api/controllers/admin_api_controller.ex`) for
//! the credential and security actions, rendering through `AdminAPI.AccountView`
//! (`lib/pleroma/web/admin_api/views/account_view.ex`). Pleroma gates the
//! account reads with `admin:read:accounts` and every write — and its
//! issued reset token — with `admin:write:accounts`, except the forced
//! follow pair (`admin:write:follows`); the same gates are the
//! [`AdminUser::require`] call each handler makes first, on the area
//! `accounts`. The listing and show answer Pleroma's admin account entity,
//! [`AdminAccount`].
//!
//! - `GET /users` (`index`) — [`Search.user`](crate::models::user::admin)
//!   over the request's criteria: `query` (a case-insensitive substring of
//!   nickname, display name or email), `filters` (the comma list of `local`,
//!   `external`, `active`, `deactivated`, `need_approval`, `unconfirmed`,
//!   `is_admin`, `is_moderator` — a name off that list is ignored),
//!   `tags[]`, `name`, `email` and `actor_types[]`, paged by `page` (from 1)
//!   and `page_size` (Pleroma's 50, capped at 200). The answer is
//!   `{users, count, page_size}`: the nickname-ordered page, the total the
//!   criteria select across every page, and the size the page was cut to.
//!   The instance actor is never among them.
//! - `GET /users/{nickname}` (`show`) — the account the segment names, by
//!   nickname (a remote `nick@domain` handle included, axum delivering the
//!   percent-decoded segment) or, when no nickname matches, by id. Anything
//!   else is the 404 `{"error":"Not found"}` the rest of this API answers
//!   with; unlike the listing, the show finds a deactivated account and the
//!   instance actor, because naming one is not listing them.
//! - `GET /users/{nickname}/credentials` — the moderation form as
//!   Pleroma's `"credentials.json"` renders it: [`AdminCredentials`], the
//!   twenty-four editable keys of one LOCAL account (the ticket's scope: a
//!   remote mirror has no credentials to administer and answers 404 like a
//!   miss).
//! - `PATCH /users/{nickname}/credentials` — Pleroma's
//!   `update_user_credentials`: `email`, `password` (with
//!   `password_confirmation`) and the profile fields — `name`, `bio`,
//!   `raw_fields`, `default_scope`, `actor_type`, `pleroma_settings_store`
//!   and the preference booleans — each validated by the user-facing
//!   `update_credentials`' own rules, refused Ecto-style as the 400
//!   `{"errors":{"<field>":["<message>", …]}}` naming EVERY refusing field.
//!   A new password replaces the hash and revokes every token in one
//!   transaction; an explicit `pleroma_settings_store` merges over the
//!   stored blob; the accepted write logs an `updated_users` entry and
//!   answers `{"status":"success"}`. Unlike Pleroma's async
//!   `force_password_reset` behind the password change, the account is not
//!   put on pending reset: the admin just set the password itself.
//! - `GET /users/{nickname}/password_reset` — Pleroma's
//!   `get_password_reset`: mints a reset token for the LOCAL account and
//!   answers `{token, link}`, the link the `POST
//!   /api/v1/pleroma/password_reset` form spends like the emailed one.
//!   Gated `admin:write:accounts` as Pleroma's scope pair has it, and
//!   logged nowhere: it is a read-shaped mint, and Pleroma does not log it.
//! - `PATCH /users/force_password_reset` (Pleroma's path) and its alias
//!   `PATCH /users/password_reset` (`force_password_reset`) — `{"nicknames"
//!   : […]}`: every named account gets `password_reset_pending` raised and
//!   its tokens revoked in one transaction (the pending flag is what refuses
//!   its password-grant logins until a new password lands), unknown names
//!   are skipped Pleroma-style, the subjects log one entry and the answer
//!   is 204.
//! - `PATCH /users/confirm_email` — `{"nicknames": […]}`: confirms every
//!   named account's address without its token (and retires any pending
//!   token), logs one entry, 204 where Pleroma answers an empty 200.
//! - `PATCH /users/resend_confirmation_email` — `{"nicknames": […]}`:
//!   queues the confirmation letter again for the named accounts whose
//!   address is still unconfirmed (a confirmed one is left alone), logs one
//!   entry for every resolved subject, 204.
//! - `PUT /users/disable_mfa` — `{"nickname": …}`: resets the named
//!   account's `mfa_settings` to the migration default — secret, codes and
//!   the enabled flag all gone — logs a `disable_mfa` entry and answers the
//!   nickname as Pleroma's `json(conn, nickname)`. An unknown name is the
//!   module's 404.
//! - `POST /users/follow` and `POST /users/unfollow` —
//!   `{"follower": …, "followed": …}`, both named by nickname, the follower
//!   required local: the admin forces the relationship through the
//!   [`follow`] model, accepting a `pending` request on the spot (Pleroma's
//!   `User.follow(follower, followed, :follow_accept)`) and running the
//!   user-facing endpoint's notification and federation hook, so a locked
//!   or remote target is followed without an answer of its own. Each logs a
//!   `"@admin made @follower follow|unfollow @followed"` entry and answers
//!   the JSON string `"ok"`.
//!
//! WBE-197 adds the profile tags, the staff groups and the suggestion pool.
//!
//! - `PUT /users/tag` and `DELETE /users/tag` — the request's `nicknames[]`
//!   and `tags[]`, in the body or the query string: every account the
//!   request names gains — or loses — those profile tags, deduped and
//!   lowercased into the spelling the [`crate::federation::mrf::tag`]
//!   switches match by (Pleroma's `User.tag` / `User.untag`, both
//!   idempotent: tagging a held tag and untagging an absent one change
//!   nothing and stay 204). Unknown handles are skipped and mirrors are
//!   taggable — the `mrf_tag:` tags exist to be put on remote accounts.
//!   Each writes a `tag` / `untag` entry naming the accounts and the tags
//!   and answers 204. Pleroma's `GET /users/tag` tag listing is not part of
//!   this router, per the ticket.
//! - `GET /users/{nickname}/permission_group` and its
//!   `/{group}` twin — Pleroma's `right_get`: the account's two flags as
//!   `{"is_moderator": …, "is_admin": …}`, mirror included, the group
//!   segment read as nothing but validated (an unknown one is the 404).
//! - `POST` and `DELETE /users/{nickname}/permission_group/{group}` —
//!   `right_add` and `right_delete`: one LOCAL account joins or leaves one
//!   group, the answer naming the one column that moved
//!   (`{"is_admin": true}`, `{"is_moderator": false}`, …) and one `grant` /
//!   `revoke` entry logged. A mirror is the 400 `{"error":"Can't set role
//!   for a remote user"}` and the admin who names their own account in the
//!   revoke is the 403 `{"error":"You can't revoke your own admin status."}`,
//!   for either group.
//! - `POST` and `DELETE /users/permission_group/{group}` — the same write
//!   over a `nicknames[]` list: everyone but the caller and the mirrors
//!   moves (`right_delete_multiple`'s self-refusal is the ticket's
//!   self-exclusion here), one entry names them, and the answer is the
//!   group's column carrying the state it was written to.
//! - `PATCH /users/suggest` and `PATCH /users/unsuggest` — a `nicknames[]`
//!   list in or out of the suggestion pool the
//!   `GET /api/v{1,2}/suggestions` listings answer from (Pleroma's
//!   `UserController.suggest` / `unsuggest`), answered with the moved
//!   accounts as `{"users": [admin entity…]}` and logged as an
//!   `add_suggestion` / `remove_suggestion` entry.
//!
//! WBE-196 adds the account lifecycle: creation, deletion, activation and
//! approval.
//!
//! - `POST /users` — `{"users": [{"nickname", "email", "password"}…]}`,
//!   at most 50 of them (more is refused whole, nothing
//!   created). Each item goes through [`create_local_user`] with the
//!   confirmation requirement bypassed — the admin set the password and
//!   vouches for the address, so no letter is queued (Pleroma's
//!   `confirmed: true`) — and the approval gate lifted off the row when
//!   `REGISTRATIONS_APPROVAL_REQUIRED` set it: every created account is
//!   active, confirmed and approved whatever the instance's modes. The
//!   answer is always 200 with one entry per item: Pleroma's
//!   `"created.json"` map (`code` 200, `type` `"success"`, `data`
//!   `{email, nickname}`) for a stored account, or its
//!   `"create_error.json"` shape with the ticket's `code` 400, the
//!   refused field map as the `error` text and the submitted handle and
//!   address as `data`. Where Pleroma rolls the whole batch back into one
//!   409, the ticket pins per-item results, so one refused item leaves
//!   its siblings stored; the accounts that were created log one
//!   `create` entry.
//! - `DELETE /users` — `nicknames[]` in the body or the query: every
//!   named account that exists starts its deletion the way the
//!   user-facing `POST /api/pleroma/delete_account` does — `deleted_at`
//!   stamped, every token revoked, the `users.delete` sweep queued on
//!   the `background` queue (the sweep's first act is the `Delete`
//!   announcement to the peers) — except the caller's own row, which is
//!   ignored. That start is [`helpers::start_deletion`], shared with the
//!   Mastodon admin API's `DELETE /api/v1/admin/accounts/{id}`. The answer
//!   is the JSON array of the nicknames started, Pleroma's
//!   `json(conn, nicknames)` narrowed to what was found and not the
//!   caller, and one `delete` entry names them.
//! - `PATCH /users/activate` and `PATCH /users/deactivate` —
//!   `nicknames[]`: the `is_active` flag moves on every named account
//!   that exists — the acting admin's own excepted from the
//!   DEACTIVATION (Pleroma's `UserController.activate` / `deactivate`,
//!   with the self-protection the ticket pins). A deactivation is the
//!   flag alone: tokens keep existing (the bearer gate answers their
//!   owner's requests a 403, the login gate refuses the password grant)
//!   and nothing federates — the actor document must stay resolvable.
//!   The answer is the moved accounts as admin entities under `users`,
//!   logged one `activate` / `deactivate` entry.
//! - `PATCH /users/{nickname}/toggle_activation` — Pleroma's
//!   `toggle_activation`: flips one account's flag, answers its single
//!   admin entity and logs the move it made under the other pair's
//!   actions. Like the bulk deactivation it protects the caller: toggling
//!   the caller's own active account is a 403 and writes nothing.
//! - `PATCH /users/approve` — `nicknames[]`: every named account still
//!   awaiting staff clears `approval_pending` and gets the
//!   `account_approved` letter; an already-approved one answers unchanged
//!   and is mailed nothing (Pleroma's `approve` clause for
//!   `is_approved: true`), and no credential is minted — the approved
//!   account signs in with its own password. The clearing write and its
//!   letter are [`helpers::approve_account`], shared with the Mastodon
//!   admin API's `POST /api/v1/admin/accounts/{id}/approve` (WBE-213). The
//!   answer is the named accounts as admin entities under `users`, one
//!   `approve` entry naming them.
//!
//! Two of Pleroma's parameters are read wider here than its
//! `CastAndValidate` declares them: an array key arrives in both the
//! bracketed spelling `pleroma-fe` sends (`tags[]`) and the bare one
//! (`tags`), the way the timeline routes read their hashtag lists. The
//! lifecycle `nicknames[]` of WBE-196 reads both spellings from the body
//! OR the query string, the way the tag pair does, because its `DELETE
//! /users` is specified as "body or query".
//!
//! [`AdminUser::require`]: crate::auth::admin::AdminUser::require
//! [`AdminAccount`]: crate::entities::admin_account::AdminAccount
//! [AdminCredentials]: crate::entities::admin_account::AdminCredentials
//! [`create_local_user`]: crate::models::user::register::create_local_user
//! [`follow`]: crate::models::follow
//! [`AdminCredentials`]: crate::entities::admin_account::AdminCredentials
//! [`helpers::start_deletion`]: crate::api::helpers::start_deletion
//! [`helpers::approve_account`]: crate::api::helpers::approve_account

use axum::{
    Json, Router,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, patch, post, put},
};
use serde_json::json;

use crate::{
    error::{AppError, AppResult},
    models::user::{User, admin, register},
    state::AppState,
};

/// The read endpoint class of [`AdminUser::require`].
const CLASS_READ: &str = "read";
/// The write endpoint class of [`AdminUser::require`]: every maintenance
/// action of this module is gated by it, the credentials read by
/// [`CLASS_READ`] (Pleroma's `OAuthScopesPlug` pair for each route).
const CLASS_WRITE: &str = "write";
/// The admin scope area these endpoints live in
/// (`admin:read:accounts`, Pleroma's `OAuthScopesPlug` pair for `index` and
/// `show`).
const AREA_ACCOUNTS: &str = "accounts";
/// The admin scope area of the forced follow and unfollow
/// (`admin:write:follows`, Pleroma's `OAuthScopesPlug` for `follow` and
/// `unfollow`); `admin:write:accounts` does not authorize them.
const AREA_FOLLOWS: &str = "follows";
/// Ecto's inclusion message, the refusal [`SCOPES`] and [`ACTOR_TYPES`]
/// answer a value outside their list with.
const INCLUSION: &str = "is not included in the list";
/// Ecto's `format` message for an email that is no plausible address.
const HAS_INVALID_FORMAT: &str = "has invalid format";
/// Ecto's `unique_constraint` message for an address another account holds.
const HAS_ALREADY_TAKEN: &str = "has already been taken";
/// Ecto's `validate_required` message for a blank password.
const CANNOT_BE_BLANK: &str = "can't be blank";
/// Ecto's `validate_confirmation` message for a password the confirmation
/// does not match.
const DOES_NOT_MATCH: &str = "does not match password";
/// Ecto's `cast` message for a `pleroma_settings_store` that is neither a
/// JSON object nor the JSON text of one.
const STORE_INVALID: &str = "is invalid";
/// The credentials write's answer: Pleroma's `%{"status" => "success"}`.
const SUCCESS: &str = "success";
/// The follow pair's answer: the ticket pins the bare JSON string `"ok"`
/// where Pleroma renders its own success map.
const OK: &str = "ok";
/// 404 body of a `permission_group` segment that names neither `admin` nor
/// `moderator`: Pleroma's `right_add`, `right_delete` and their bulk twins
/// all answer an unknown group with `render_error(conn, :not_found, "No
/// such permission_group")`. The ticket pins the status; the wording is the
/// reference's, and the module's generic `RouteNotFound` "Not found" stays
/// for the misses that name an account.
const NO_SUCH_GROUP: &str = "No such permission_group";
/// 400 body of a role write aimed at a remote mirror: a role is this
/// instance's own to give, and the ticket pins this wording for both the
/// grant and the revoke.
const REMOTE_ROLE: &str = "Can't set role for a remote user";
/// 403 body of the admin who revokes a role from their own account:
/// Pleroma's `right_delete` self clause.
const SELF_REVOKE: &str = "You can't revoke your own admin status.";
/// The largest batch `POST /users` may create in one request, the cap the
/// ticket pins; a bigger one is refused whole, before anything is created.
const MAX_CREATE_USERS: usize = 50;

/// The user routes of the admin API, merged into [`super`].
///
/// `/users/{nickname}` is the show route of the same prefix the invite
/// actions (`/users/invites` and friends, [`super::invites`]) live under;
/// the router prefers a static segment over a parameter, so both are
/// reachable. The same is true of the maintenance actions of WBE-198
/// (`/users/password_reset`, `/users/confirm_email`,
/// `/users/resend_confirmation_email`, `/users/disable_mfa`,
/// `/users/follow` and `/users/unfollow`) beside the two `{nickname}`
/// routes the credentials read and the issued reset token hang on, of
/// this ticket's static `/users/tag`, `/users/suggest` and
/// `/users/unsuggest`, and of WBE-196's `/users/activate`,
/// `/users/deactivate` and `/users/approve` beside the single account's
/// `/users/{nickname}/toggle_activation`.
///
/// The two `permission_group` shapes coexist the same way: the bulk pair
/// `/users/permission_group/{group}` hangs off the static
/// `permission_group` segment the single account's
/// `/users/{nickname}/permission_group[/{group}]` reads occupy as a
/// parameter, and the static one wins the exact-match contest.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/users",
            get(list_users).post(create_users).delete(delete_users),
        )
        .route("/users/{nickname}", get(show_user))
        .route(
            "/users/{nickname}/credentials",
            get(show_user_credentials).patch(update_user_credentials),
        )
        .route("/users/{nickname}/password_reset", get(get_password_reset))
        .route("/users/password_reset", patch(force_password_reset))
        .route("/users/force_password_reset", patch(force_password_reset))
        .route("/users/confirm_email", patch(confirm_email_users))
        .route(
            "/users/resend_confirmation_email",
            patch(resend_confirmation_email),
        )
        .route("/users/disable_mfa", put(disable_mfa))
        .route("/users/follow", post(follow_users))
        .route("/users/unfollow", post(unfollow_users))
        .route("/users/tag", put(tag_users).delete(untag_users))
        .route(
            "/users/{nickname}/permission_group",
            get(show_permission_group),
        )
        .route(
            "/users/{nickname}/permission_group/{group}",
            get(show_permission_group_of)
                .post(grant_role)
                .delete(revoke_role),
        )
        .route(
            "/users/permission_group/{group}",
            post(grant_role_bulk).delete(revoke_role_bulk),
        )
        .route("/users/suggest", patch(suggest_users))
        .route("/users/unsuggest", patch(unsuggest_users))
        .route("/users/activate", patch(activate_users))
        .route("/users/deactivate", patch(deactivate_users))
        .route("/users/approve", patch(approve_users))
        .route(
            "/users/{nickname}/toggle_activation",
            patch(toggle_activation),
        )
}

/// The batch create.
mod create;
/// The credentials read and write.
mod credentials;
/// The credentials write's email and password checks and the apply step.
mod credentials_apply;
/// The credentials patch's field checks.
mod credentials_checks;
/// The permission-group reads and writes and the suggestion pair.
mod groups;
/// The account lifecycle writes.
mod lifecycle;
/// The users listing and the single-account read.
mod listing;
/// The maintenance actions.
mod maintenance;
/// The tag pair and the helpers the tag and group writes share.
mod tags;
/// Fixtures more than one child's tests build on.
#[cfg(test)]
mod test_support;

use create::create_users;
use credentials::{show_user_credentials, update_user_credentials};
use groups::{
    grant_role, grant_role_bulk, revoke_role, revoke_role_bulk, show_permission_group,
    show_permission_group_of, suggest_users, unsuggest_users,
};
use lifecycle::{activate_users, approve_users, deactivate_users, delete_users, toggle_activation};
use listing::{list_users, show_user};
use maintenance::{
    confirm_email_users, disable_mfa, follow_users, force_password_reset, get_password_reset,
    resend_confirmation_email, unfollow_users,
};
use tags::{tag_users, untag_users};

/// What the credentials and security writes can fail with: the project
/// error as-is, or the changeset refusal the ticket pins.
#[derive(Debug)]
enum UserError {
    /// An ordinary [`AppError`], rendered by its own `IntoResponse`.
    App(AppError),
    /// The 400 `{"errors":{"<field>":["<message>", …]}}` Pleroma's Ecto
    /// changeset renders: every refusing field with all of its messages,
    /// because `update_user_credentials` validates the whole body at once.
    Invalid(register::FieldErrors),
}

/// The handlers' result with [`UserError`] as the error half.
type UserResult<T> = Result<T, UserError>;

/// A [`UserError`] is a response: the wrapped [`AppError`] renders itself,
/// the invalid arm the pinned `{"errors": …}` 400 — the same shape
/// [`super::webhooks`]' local error answers with, widened to the
/// many-messages-per-field map a full changeset run produces.
impl IntoResponse for UserError {
    fn into_response(self) -> Response {
        match self {
            Self::App(err) => err.into_response(),
            Self::Invalid(errors) => {
                (StatusCode::BAD_REQUEST, Json(json!({ "errors": errors }))).into_response()
            }
        }
    }
}

/// Every [`AppError`] the extractors, scope gates and models raise travels
/// through unchanged.
impl From<AppError> for UserError {
    fn from(err: AppError) -> Self {
        Self::App(err)
    }
}

/// Appends `message` to `field`'s list in one changeset run's error map.
fn refused(errors: &mut register::FieldErrors, field: &'static str, message: impl Into<String>) {
    errors.entry(field).or_default().push(message.into());
}

/// The local account one credentials or reset path segment names, by
/// nickname or id like the show: a remote mirror has no credentials to
/// administer, and the ticket sends mirrors and misses to the same 404.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] for text that names neither;
/// [`AppError::Database`] from the read.
async fn local_account(state: &AppState, raw: &str) -> AppResult<User> {
    admin::get_by_nickname_or_id(&state.db, raw)
        .await?
        .filter(|found| found.local.get())
        .ok_or(AppError::RouteNotFound)
}
