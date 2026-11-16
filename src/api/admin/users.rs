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
//! issued reset token — with `admin:write:accounts`; the same gates are the
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
//! - `PATCH /users/password_reset` (`force_password_reset`) — `{"nicknames"
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
//!   ignored. The answer is the JSON array of the nicknames started,
//!   Pleroma's `json(conn, nicknames)` narrowed to what was found and
//!   not the caller, and one `delete` entry names them.
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
//!   actions. Unlike the bulk pair it does not protect the caller: the
//!   segment that names an account names it.
//! - `PATCH /users/approve` — `nicknames[]`: every named account still
//!   awaiting staff clears `approval_pending` and gets the
//!   `account_approved` letter; an already-approved one answers unchanged
//!   and is mailed nothing (Pleroma's `approve` clause for
//!   `is_approved: true`), and no credential is minted — the approved
//!   account signs in with its own password. The answer is the named
//!   accounts as admin entities under `users`, one `approve` entry
//!   naming them. `POST /api/v1/admin/accounts/{id}/approve` is the
//!   separate Mastodon-admin ticket's own route.
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

use std::{fmt, slice};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, patch, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::debug;

use crate::{
    api::{
        accounts::{
            ACTOR_TYPES, SCOPES, SERVICE_ACTOR_TYPE, after_follow_changed, after_profile_updated,
            render_note, sanitized_name, stored_fields,
        },
        helpers::QueryPairs,
        password_reset::RESET_PATH,
        pleroma_settings::{check_store_size, shallow_merge},
    },
    auth::admin::AdminUser,
    config::Config,
    entities::admin_account::{
        AdminAccount, AdminAccountList, AdminAccounts, AdminCredentials, AdminPermissionGroup,
    },
    error::{AppError, AppResult},
    extract::{ApiBody, ApiParams, flex},
    jobs::{QUEUE_BACKGROUND, deletion},
    mailer::{self, templates},
    models::{
        follow::{self, FollowState},
        job::{NewJob, enqueue},
        moderation_log, oauth_token, password_reset,
        user::{self, Flag, PleromaPrefs, ProfileFields, User, admin, confirmation, mfa, register},
    },
    state::AppState,
    util::password::hash_password,
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

/// `GET /api/v1/pleroma/admin/users` — Pleroma's `index`: the criteria of
/// the query string, one page of entities and the whole total.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::Database`] from the listing read.
async fn list_users(
    State(state): State<AppState>,
    admin: AdminUser,
    uri: Uri,
) -> AppResult<Json<AdminAccountList>> {
    admin.require(CLASS_READ, AREA_ACCOUNTS)?;
    let params = list_params(uri.query());
    let page = admin::list(&state.db, &params).await?;
    debug!(
        page = params.page,
        page_size = params.page_size,
        count = page.count,
        listed = page.users.len(),
        "admin users listed"
    );
    Ok(Json(AdminAccountList::from_page(
        &page,
        params.page_size,
        &state.config,
    )))
}

/// `GET /api/v1/pleroma/admin/users/{nickname}` — Pleroma's `show`: the
/// entity behind the handle or id the path segment names.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::RouteNotFound`] for text that is
/// no stored nickname and no id; [`AppError::Database`] from the read.
async fn show_user(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminAccount>> {
    admin.require(CLASS_READ, AREA_ACCOUNTS)?;
    let found = admin::get_by_nickname_or_id(&state.db, &raw)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    debug!(
        id = %found.id,
        nickname = %found.nickname,
        "admin user shown"
    );
    Ok(Json(AdminAccount::from_user(&found, &state.config)))
}

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

/// `GET /api/v1/pleroma/admin/users/{nickname}/credentials` — the
/// moderation form's own entity: Pleroma's `"credentials.json"` over the
/// local account the segment names.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::RouteNotFound`] from
/// [`local_account`]; [`AppError::Database`] from the read.
async fn show_user_credentials(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminCredentials>> {
    admin.require(CLASS_READ, AREA_ACCOUNTS)?;
    let found = local_account(&state, &raw).await?;
    debug!(
        id = %found.id,
        nickname = %found.nickname,
        "admin credentials shown"
    );
    Ok(Json(AdminCredentials::from_user(&found, &state.config)))
}

/// The parameters of `PATCH /api/v1/pleroma/admin/users/{nickname}/credentials`:
/// the columns Pleroma's `update_as_admin` owns — the profile fields of the
/// user-facing [`update_credentials`] plus `email`, `password` and its
/// confirmation — every one of them optional, a JSON `null` reading as
/// absent exactly as Pleroma drops nil-valued parameters before the
/// changeset.
///
/// The preference booleans arrive through [`flex::opt_bool`], so a form's
/// `"true"`/`"on"`/`"1"` spells the same as JSON `true` and an unknown
/// string is the extractor's 422; `raw_fields` and `pleroma_settings_store`
/// stay [`Value`]s so every shape survives the body parser and
/// [`stored_fields`] / [`checked_store`] fold them.
///
/// [`update_credentials`]: crate::api::accounts
#[derive(Deserialize)]
struct CredentialsParams {
    /// The display name: [`sanitized_name`]'s rules, refused under `name`.
    #[serde(default)]
    name: Option<String>,
    /// The profile note: the user endpoint's length rule, then
    /// [`render_note`]'s rendering, refused under `bio`.
    #[serde(default)]
    bio: Option<String>,
    /// The profile metadata, [`stored_fields`]' input, refused under
    /// `raw_fields`.
    #[serde(default)]
    raw_fields: Option<Value>,
    /// The address to store, format- and uniqueness-checked.
    #[serde(default)]
    email: Option<String>,
    /// The visibility new statuses default to, one of [`SCOPES`].
    #[serde(default)]
    default_scope: Option<String>,
    /// The actor kind, one of [`ACTOR_TYPES`]; it also re-decides `bot`.
    #[serde(default)]
    actor_type: Option<String>,
    /// The client settings patch, shallow-merged over the stored blob.
    #[serde(default)]
    pleroma_settings_store: Option<Value>,
    /// Whether follow requests need manual approval.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    is_locked: Option<bool>,
    /// Whether the account is in the discovery pool.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    is_discoverable: Option<bool>,
    /// Whether clients should render the account's content as plain text.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    no_rich_text: Option<bool>,
    /// Whether the follow collection is hidden from other accounts.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    hide_follows: Option<bool>,
    /// Whether the follower collection is hidden from other accounts.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    hide_followers: Option<bool>,
    /// Whether the follower count renders as zero for others.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    hide_followers_count: Option<bool>,
    /// Whether the follow count renders as zero for others.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    hide_follows_count: Option<bool>,
    /// Whether the favourites list is private to the account.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    hide_favorites: Option<bool>,
    /// Whether a `Move` of the account may take the following list with it.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    allow_following_move: Option<bool>,
    /// Whether the staff flags may be shown to other accounts.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    show_role: Option<bool>,
    /// Whether the account's statuses skip thread containment.
    #[serde(default, deserialize_with = "flex::opt_bool")]
    skip_thread_containment: Option<bool>,
    /// The new password, in plain text — length-checked, hashed on apply.
    #[serde(default)]
    password: Option<String>,
    /// The confirmation of [`password`](Self::password); present-and-
    /// different is the refusal, absent is Pleroma's overwrite semantics
    /// (the admin does not have to type the password twice).
    #[serde(default)]
    password_confirmation: Option<String>,
}

/// Neither `password` nor `password_confirmation` ever renders: the body
/// carries the plaintext the admin (or a leaked form) chose.
impl fmt::Debug for CredentialsParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialsParams")
            .field("name", &self.name)
            .field("bio", &self.bio)
            .field("raw_fields", &self.raw_fields)
            .field("email", &self.email)
            .field("default_scope", &self.default_scope)
            .field("actor_type", &self.actor_type)
            .field("pleroma_settings_store", &self.pleroma_settings_store)
            .field("is_locked", &self.is_locked)
            .field("is_discoverable", &self.is_discoverable)
            .field("no_rich_text", &self.no_rich_text)
            .field("hide_follows", &self.hide_follows)
            .field("hide_followers", &self.hide_followers)
            .field("hide_followers_count", &self.hide_followers_count)
            .field("hide_follows_count", &self.hide_follows_count)
            .field("hide_favorites", &self.hide_favorites)
            .field("allow_following_move", &self.allow_following_move)
            .field("show_role", &self.show_role)
            .field("skip_thread_containment", &self.skip_thread_containment)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field(
                "password_confirmation",
                &self.password_confirmation.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl CredentialsParams {
    /// Whether the body carries any of the preference booleans — half of
    /// what makes a credentials PATCH a profile write at all.
    fn any_flag(&self) -> bool {
        [
            self.is_locked,
            self.is_discoverable,
            self.no_rich_text,
            self.hide_follows,
            self.hide_followers,
            self.hide_followers_count,
            self.hide_follows_count,
            self.hide_favorites,
            self.allow_following_move,
            self.show_role,
            self.skip_thread_containment,
        ]
        .into_iter()
        .any(|flag| flag.is_some())
    }
}

/// `PATCH /api/v1/pleroma/admin/users/{nickname}/credentials` — Pleroma's
/// `update_user_credentials`: validate the whole body Ecto-style, write
/// what passed and log it.
///
/// # Errors
///
/// [`UserError::Invalid`] for the field map of the refusals (see
/// [`check_credentials`]); [`AppError::Unauthorized`] /
/// [`AppError::Forbidden`] from [`AdminUser`] and its `admin:write` gate;
/// [`AppError::RouteNotFound`] from [`local_account`];
/// [`AppError::Unprocessable`] from the body extractor for an unusable
/// boolean; [`AppError::Internal`] from the password hashing; and
/// [`AppError::Database`] from any read or write.
async fn update_user_credentials(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
    body: ApiBody<CredentialsParams>,
) -> UserResult<Json<Value>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let found = local_account(&state, &raw).await?;
    let patch = check_credentials(&state, &found, &body.0).await?;
    apply_credentials(&state, &found, patch).await?;
    let logged =
        moderation_log::updated_users(&state.db, &admin.0, slice::from_ref(&found)).await?;
    debug!(
        log = %logged.id,
        id = %found.id,
        nickname = %found.nickname,
        "admin user credentials updated"
    );
    Ok(Json(json!({ "status": SUCCESS })))
}

/// A validated credentials body: the writes it asks for, with every value
/// checked, merged and rendered, and nothing left to refuse.
#[derive(Debug)]
struct CredentialsPatch {
    /// The whole profile column set for
    /// [`update_profile`](crate::models::user::update_profile), `None` when
    /// the body named no profile field at all.
    profile: Option<ProfileFields>,
    /// The new address for [`set_email`](crate::models::user::set_email),
    /// `None` for no email write.
    email: Option<String>,
    /// The new password in plain text — only its hash ever reaches a row,
    /// and it never lives past this request.
    password: Option<String>,
}

/// Runs Pleroma's `update_as_admin` changeset over the submitted body:
/// every rule answers with its Ecto message under its own field name, and a
/// body that refuses on several fields reports all of them at once. With
/// nothing refused, the checked writes come back for [`apply_credentials`].
///
/// # Errors
///
/// [`UserError::Invalid`] when any field refused; [`AppError::Database`]
/// from the uniqueness lookup and the note's mention resolution.
async fn check_credentials(
    state: &AppState,
    current: &User,
    params: &CredentialsParams,
) -> Result<CredentialsPatch, UserError> {
    let mut errors = register::FieldErrors::new();
    let profile = check_profile(state, current, params, &mut errors).await?;
    let email = check_email(state, current, params.email.as_deref(), &mut errors).await?;
    let password = check_password(&state.config, params, &mut errors);
    if !errors.is_empty() {
        return Err(UserError::Invalid(errors));
    }
    Ok(CredentialsPatch {
        profile,
        email,
        password,
    })
}

/// The profile half of the changeset: the six field checks, then — when the
/// body touched any profile column — the stored row with every submission
/// written on top, ready for the one-statement [`update_profile`].
///
/// # Errors
///
/// [`AppError::Database`] from [`render_note`]'s mention lookups; the
/// refusals land in `errors`, not in the return.
///
/// [`update_profile`]: crate::models::user::update_profile
async fn check_profile(
    state: &AppState,
    current: &User,
    params: &CredentialsParams,
    errors: &mut register::FieldErrors,
) -> AppResult<Option<ProfileFields>> {
    let checks = ProfileChecks {
        name: checked_name(params.name.as_deref(), &state.config, errors)?,
        note: checked_note(params.bio.as_deref(), &state.config, errors),
        metadata: checked_metadata(params.raw_fields.clone(), &state.config, errors)?,
        scope: checked_scope(params.default_scope.as_deref(), errors).map(str::to_owned),
        actor: checked_actor(params.actor_type.as_deref(), errors),
        store: checked_store(params.pleroma_settings_store.as_ref(), current, errors),
    };
    if checks.untouched(params) {
        return Ok(None);
    }
    Ok(Some(checks.merge_into(state, current, params).await?))
}

/// The checked profile submissions of one credentials body, `None` each for
/// the field it does not write.
#[derive(Debug)]
struct ProfileChecks {
    /// The sanitized display name for the `name` column.
    name: Option<String>,
    /// The note's submitted source text, rendered by [`merge_into`].
    note: Option<String>,
    /// The stored `(fields, raw_fields)` pair from [`stored_fields`].
    metadata: Option<(Value, Value)>,
    /// The visibility the `default_scope` column takes.
    scope: Option<String>,
    /// The `(actor_type, bot)` pair an explicit kind decides.
    actor: Option<(String, Flag)>,
    /// The merged client settings blob.
    store: Option<Value>,
}

impl ProfileChecks {
    /// Whether this body writes no profile column at all: nothing checked,
    /// no flag stated.
    fn untouched(&self, params: &CredentialsParams) -> bool {
        self.name.is_none()
            && self.note.is_none()
            && self.metadata.is_none()
            && self.scope.is_none()
            && self.actor.is_none()
            && self.store.is_none()
            && !params.any_flag()
    }

    /// The stored row's [`ProfileFields`] — every column carrying its stored
    /// value, since a credentials PATCH rewrites the whole set at once —
    /// with every checked submission written on top. Only the note needs a
    /// database read to render, so it lands last.
    ///
    /// # Errors
    ///
    /// [`AppError::Database`] from [`render_note`].
    async fn merge_into(
        self,
        state: &AppState,
        current: &User,
        params: &CredentialsParams,
    ) -> AppResult<ProfileFields> {
        let mut profile = stored_profile(current);
        if let Some(name) = self.name {
            profile.name = name;
        }
        if let Some(raw) = self.note.as_deref() {
            profile.bio = render_note(state, raw).await?;
            profile.raw_bio = raw.to_owned();
        }
        if let Some((fields, raw_fields)) = self.metadata {
            profile.fields = fields;
            profile.raw_fields = raw_fields;
        }
        if let Some(scope) = self.scope {
            profile.default_scope = scope;
        }
        if let Some((kind, automated)) = self.actor {
            profile.actor_type = kind;
            profile.bot = automated;
        }
        if let Some(store) = self.store {
            profile.settings_store = store;
        }
        merge_flags(params, &mut profile);
        Ok(profile)
    }
}

/// The stored row as a complete [`ProfileFields`]: the shape
/// [`update_profile`] wants when a request changes columns one by one.
///
/// [`update_profile`]: crate::models::user::update_profile
fn stored_profile(current: &User) -> ProfileFields {
    ProfileFields {
        name: current.name.clone(),
        bio: current.bio.clone(),
        raw_bio: current.raw_bio.clone(),
        locked: current.locked,
        bot: current.bot,
        discoverable: current.discoverable,
        fields: current.fields.clone(),
        raw_fields: current.raw_fields.clone(),
        avatar_url: current.avatar_url.clone(),
        banner_url: current.banner_url.clone(),
        background_image_url: current.background_image_url.clone(),
        default_scope: current.default_scope.clone(),
        sensitive_default: current.sensitive_default,
        language: current.language.clone(),
        pleroma: stored_flags(current),
        birthday: current.birthday,
        settings_store: current.settings_store.clone(),
        actor_type: current.actor_type.clone(),
    }
}

/// The eleven `pleroma` preference columns as the row holds them; the nine
/// a credentials body owns are overwritten by [`merge_flags`].
const fn stored_flags(current: &User) -> PleromaPrefs {
    PleromaPrefs {
        hide_followers: current.hide_followers,
        hide_follows: current.hide_follows,
        hide_followers_count: current.hide_followers_count,
        hide_follows_count: current.hide_follows_count,
        hide_favorites: current.hide_favorites,
        show_role: current.show_role,
        skip_thread_containment: current.skip_thread_containment,
        allow_following_move: current.allow_following_move,
        accepts_chat_messages: current.accepts_chat_messages,
        no_rich_text: current.no_rich_text,
        show_birthday: current.show_birthday,
    }
}

/// Writes the stated preference booleans over the merged profile: the two
/// named `is_…` in the body's spelling, the nine in the entity's.
fn merge_flags(params: &CredentialsParams, profile: &mut ProfileFields) {
    profile.locked = params.is_locked.map_or(profile.locked, Flag::from);
    profile.discoverable = params
        .is_discoverable
        .map_or(profile.discoverable, Flag::from);
    let pleroma = &mut profile.pleroma;
    pleroma.hide_followers = params
        .hide_followers
        .map_or(pleroma.hide_followers, Flag::from);
    pleroma.hide_follows = params.hide_follows.map_or(pleroma.hide_follows, Flag::from);
    pleroma.hide_followers_count = params
        .hide_followers_count
        .map_or(pleroma.hide_followers_count, Flag::from);
    pleroma.hide_follows_count = params
        .hide_follows_count
        .map_or(pleroma.hide_follows_count, Flag::from);
    pleroma.hide_favorites = params
        .hide_favorites
        .map_or(pleroma.hide_favorites, Flag::from);
    pleroma.show_role = params.show_role.map_or(pleroma.show_role, Flag::from);
    pleroma.skip_thread_containment = params
        .skip_thread_containment
        .map_or(pleroma.skip_thread_containment, Flag::from);
    pleroma.allow_following_move = params
        .allow_following_move
        .map_or(pleroma.allow_following_move, Flag::from);
    pleroma.no_rich_text = params.no_rich_text.map_or(pleroma.no_rich_text, Flag::from);
}

/// The checked display name: [`sanitized_name`] ran the user endpoint's
/// length rule and stripping, and its refusal lands under `name`.
///
/// # Errors
///
/// [`AppError::Database`] never (the check is pure); any non-`Unprocessable`
/// failure the helper could one day carry still aborts the request.
fn checked_name(
    submitted: Option<&str>,
    config: &Config,
    errors: &mut register::FieldErrors,
) -> AppResult<Option<String>> {
    let Some(raw) = submitted else {
        return Ok(None);
    };
    match sanitized_name(raw, config.max_display_name_chars) {
        Ok(clean) => Ok(Some(clean)),
        Err(AppError::Unprocessable(message)) => {
            refused(errors, "name", message);
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

/// The note's submission after the user endpoint's length rule, whose
/// message lands under `bio`; the rendering waits for [`merge_into`].
fn checked_note(
    submitted: Option<&str>,
    config: &Config,
    errors: &mut register::FieldErrors,
) -> Option<String> {
    let raw = submitted?;
    if raw.chars().count() > config.max_bio_chars {
        refused(
            errors,
            "bio",
            format!(
                "Bio is too long (maximum is {} characters)",
                config.max_bio_chars
            ),
        );
        return None;
    }
    Some(raw.to_owned())
}

/// The checked metadata pair: [`stored_fields`] ran the count and length
/// rules, and its refusal lands under `raw_fields` — the body's own name
/// for the input.
///
/// # Errors
///
/// None in practice: the helper's only failure is the collected
/// `Unprocessable`, but any other still aborts the request.
fn checked_metadata(
    submitted: Option<Value>,
    config: &Config,
    errors: &mut register::FieldErrors,
) -> AppResult<Option<(Value, Value)>> {
    let Some(raw) = submitted else {
        return Ok(None);
    };
    match stored_fields(raw, config.max_profile_fields) {
        Ok(pair) => Ok(Some(pair)),
        Err(AppError::Unprocessable(message)) => {
            refused(errors, "raw_fields", message);
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

/// The checked visibility: any text outside [`SCOPES`] is Ecto's inclusion
/// message under `default_scope`, and `None` keeps the stored one.
fn checked_scope<'a>(
    submitted: Option<&'a str>,
    errors: &mut register::FieldErrors,
) -> Option<&'a str> {
    match submitted {
        Some(scope) if SCOPES.contains(&scope) => Some(scope),
        Some(_) => {
            refused(errors, "default_scope", INCLUSION);
            None
        }
        None => None,
    }
}

/// The checked actor kind: outside [`ACTOR_TYPES`] the same inclusion
/// message under `actor_type`, inside it the pair the columns keep in step
/// — Pleroma's admin write re-decides `bot` from the kind.
fn checked_actor(
    submitted: Option<&str>,
    errors: &mut register::FieldErrors,
) -> Option<(String, Flag)> {
    match submitted {
        Some(kind) if ACTOR_TYPES.contains(&kind) => {
            Some((kind.to_owned(), Flag::new(kind == SERVICE_ACTOR_TYPE)))
        }
        Some(_) => {
            refused(errors, "actor_type", INCLUSION);
            None
        }
        None => None,
    }
}

/// The checked client settings blob: the submitted object (or the JSON text
/// of one, from a form) shallow-merged over the stored blob exactly as the
/// user endpoint merges it, then measured. A value that is no object is
/// Ecto's `is invalid`, a merged blob over the cap is `is too large` — both
/// under `pleroma_settings_store`, and both leaving the stored blob
/// untouched because the whole request is refused.
fn checked_store(
    submitted: Option<&Value>,
    current: &User,
    errors: &mut register::FieldErrors,
) -> Option<Value> {
    let patch = submitted?;
    let Value::Object(map) = store_value(patch) else {
        refused(errors, "pleroma_settings_store", STORE_INVALID);
        return None;
    };
    let merged = shallow_merge(&current.settings_store, &map);
    if check_store_size(&merged).is_ok() {
        return Some(merged);
    }
    refused(errors, "pleroma_settings_store", "is too large");
    None
}

/// The value a submitted `pleroma_settings_store` names once a form's JSON
/// text has been parsed: the text of an object becomes the object, text
/// that is no JSON folds to [`Value::Null`], and every other value comes
/// back as it stands for [`checked_store`]'s object test to refuse.
fn store_value(submitted: &Value) -> Value {
    if let Value::String(text) = submitted {
        return serde_json::from_str::<Value>(text).unwrap_or(Value::Null);
    }
    submitted.clone()
}

/// The checked address: [`is_plausible_email`] first, then the uniqueness
/// read Pleroma's `unique_constraint(:email)` stands in for — held by
/// another account is taken, held by this one is a no-op rewrite.
///
/// # Errors
///
/// [`AppError::Database`] from the uniqueness read.
async fn check_email(
    state: &AppState,
    current: &User,
    submitted: Option<&str>,
    errors: &mut register::FieldErrors,
) -> AppResult<Option<String>> {
    let Some(email) = submitted else {
        return Ok(None);
    };
    if !register::is_plausible_email(email) {
        refused(errors, "email", HAS_INVALID_FORMAT);
        return Ok(None);
    }
    let holder = user::get_by_email(&state.db, email).await?;
    if holder.is_some_and(|other| other.id != current.id) {
        refused(errors, "email", HAS_ALREADY_TAKEN);
        return Ok(None);
    }
    Ok(Some(email.to_owned()))
}

/// The checked password, Pleroma's `password_update_changeset` rules with
/// the confirmation check the admin form keeps (see the module docs for the
/// deliberate divergence from its forced-overwrite): blank is Ecto's
/// `can't be blank`, too short the interpolated `should be at least …`, and
/// a stated confirmation that differs is refused under its own name.
/// `password_confirmation` alone, without a password, is nothing to check.
fn check_password(
    config: &Config,
    params: &CredentialsParams,
    errors: &mut register::FieldErrors,
) -> Option<String> {
    let password = params.password.as_deref()?;
    if params
        .password_confirmation
        .as_deref()
        .is_some_and(|confirmation| confirmation != password)
    {
        refused(errors, "password_confirmation", DOES_NOT_MATCH);
    }
    if password.is_empty() {
        refused(errors, "password", CANNOT_BE_BLANK);
        return None;
    }
    if password.chars().count() < config.min_password_length {
        refused(
            errors,
            "password",
            format!(
                "should be at least {} character(s)",
                config.min_password_length
            ),
        );
        return None;
    }
    Some(password.to_owned())
}

/// Runs the checked writes: the profile columns in one statement (whose
/// fresh row runs the user endpoint's own post-update hook), then the
/// address, then the password beside its token revocations in one
/// transaction — the pair Pleroma's reset `Multi` lands together.
///
/// A password write clears [`password_reset_pending`] as it goes (the
/// [`set_password`] rule): an admin who just chose the password has ended
/// any pending reset.
///
/// # Errors
///
/// [`AppError::Internal`] when the profile write matches no row (the
/// account vanished mid-request) or the hashing fails;
/// [`AppError::Database`] from any statement.
///
/// [`password_reset_pending`]: User::password_reset_pending
/// [`set_password`]: crate::models::user::set_password
async fn apply_credentials(
    state: &AppState,
    current: &User,
    patch: CredentialsPatch,
) -> AppResult<()> {
    if let Some(profile) = patch.profile.as_ref() {
        let updated = user::update_profile(&state.db, current.id, profile)
            .await?
            .ok_or_else(|| AppError::Internal("credentials update matched no row".to_owned()))?;
        after_profile_updated(state, &updated).await;
    }
    if let Some(email) = patch.email.as_deref() {
        user::set_email(&state.db, current.id, email, current.email_confirmed.get()).await?;
    }
    if let Some(password) = patch.password.as_deref() {
        let hash = hash_password(password)?;
        let mut tx = state.db.begin().await?;
        user::set_password(&mut *tx, current.id, &hash).await?;
        let revoked = oauth_token::delete_all_for_user(&mut *tx, current.id).await?;
        tx.commit().await?;
        debug!(
            revoked,
            "admin credentials write revoked the account's tokens"
        );
    }
    Ok(())
}

/// The issued reset: Pleroma's `%{token: …, link: …}` answer, the token's
/// text and the absolute URL that spends it.
#[derive(Serialize)]
pub struct PasswordResetLink {
    /// The minted token's secret text.
    token: String,
    /// The full reset URL: [`RESET_PATH`] with the token as its segment,
    /// hung on the instance's own origin.
    link: String,
}

/// Both fields are the same credential, and neither ever renders: the
/// answer's whole point is that a log line must not leak a live reset.
impl fmt::Debug for PasswordResetLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasswordResetLink")
            .field("token", &"<redacted>")
            .field("link", &"<redacted>")
            .finish()
    }
}

impl PasswordResetLink {
    /// Answers `token` and the link `{base_url}{RESET_PATH}/{token}`: the
    /// route the `POST /api/v1/pleroma/password_reset` form already serves
    /// (see [`crate::api::password_reset`]), so an admin's link completes a
    /// reset exactly like the emailed one.
    fn new(base_url: &str, token: &str) -> Self {
        Self {
            token: token.to_owned(),
            link: format!("{base_url}{RESET_PATH}/{token}"),
        }
    }
}

/// `GET /api/v1/pleroma/admin/users/{nickname}/password_reset` — Pleroma's
/// `get_password_reset`: mint a reset token for the local account and hand
/// back its text beside the link that spends it. Gated
/// `admin:write:accounts` like every other maintenance action here, and
/// left out of the moderation log: it is a mint, not a change, and Pleroma
/// does not log it either.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::RouteNotFound`] from
/// [`local_account`]; [`AppError::Database`] from the mint.
async fn get_password_reset(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<PasswordResetLink>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let found = local_account(&state, &raw).await?;
    let issued = password_reset::create_for_user(&state.db, found.id).await?;
    debug!(
        id = %found.id,
        nickname = %found.nickname,
        "admin issued a password reset token"
    );
    Ok(Json(PasswordResetLink::new(
        &state.config.base_url,
        &issued.token,
    )))
}

/// The body of the nickname-list actions: Pleroma's `nicknames` parameter,
/// required, arriving from JSON as the array and from a form as the
/// repeated `nicknames[]` key the extractors nest.
#[derive(Debug, Deserialize)]
struct NicknamesBody {
    /// The handles to act on, unknown ones skipped as Pleroma skips them.
    nicknames: Vec<String>,
}

/// The body of `PUT /users/disable_mfa`: Pleroma's single required
/// `nickname`.
#[derive(Debug, Deserialize)]
struct NicknameBody {
    /// The handle whose MFA goes away.
    nickname: String,
}

/// The parameters of the tag pair: Pleroma's `tag_users` and `untag_users`
/// both require `nicknames` AND `tags`, each arriving from JSON as the
/// array and from a form or the query string as the repeated bracketed key
/// the [`ApiParams`] extractor nests.
#[derive(Debug, Deserialize)]
struct TagsBody {
    /// The accounts whose profile tags change, unknown ones skipped.
    nicknames: Vec<String>,
    /// The tags to add to — or remove from — every one of them.
    tags: Vec<String>,
}

/// The body of the follow pair: both ends of the relationship, named by
/// nickname and both required (Pleroma's `open_api_spex` body params).
#[derive(Debug, Deserialize)]
struct FollowBody {
    /// The local account that is made to follow.
    follower: String,
    /// The account it follows — local or a remote mirror.
    followed: String,
}

/// `PATCH /api/v1/pleroma/admin/users/password_reset` — Pleroma's
/// `force_password_reset`: raise `password_reset_pending` and revoke every
/// token of each named account, in one transaction per account so a forced
/// reset can never leave a live token behind an awaiting account.
///
/// A nickname no account answers to is skipped — Pleroma maps the list
/// through its lookup and acts on what it finds, and the 204 says nothing
/// about who was not there. The subject list the log entry names is what
/// was found, which can be empty.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] from the
/// extractor for a body with no `nicknames`; [`AppError::Database`] from
/// the lookups, the writes and the log.
async fn force_password_reset(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<NicknamesBody>,
) -> AppResult<StatusCode> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let users = admin::resolve_nicknames(&state.db, &body.0.nicknames).await?;
    for found in &users {
        let mut tx = state.db.begin().await?;
        user::set_password_reset_pending(&mut *tx, found.id).await?;
        let revoked = oauth_token::delete_all_for_user(&mut *tx, found.id).await?;
        tx.commit().await?;
        debug!(
            account = %found.nickname,
            revoked,
            "admin forced a password reset"
        );
    }
    let logged = moderation_log::force_password_reset(&state.db, &admin.0, &users).await?;
    debug!(
        log = %logged.id,
        forced = users.len(),
        "admin forced password resets"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `PATCH /api/v1/pleroma/admin/users/confirm_email` — Pleroma's
/// `confirm_email`: confirm each named account's address without its token
/// (and retire any outstanding one, so a letter already in transit confirms
/// nothing). 204 where Pleroma answers an empty 200, the ticket's pinning.
///
/// # Errors
///
/// As [`force_password_reset`]'s, from the same shape of body and lookup.
async fn confirm_email_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<NicknamesBody>,
) -> AppResult<StatusCode> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let users = admin::resolve_nicknames(&state.db, &body.0.nicknames).await?;
    for found in &users {
        confirmation::set_confirmed(&state.db, found.id).await?;
    }
    let logged = moderation_log::confirm_email(&state.db, &admin.0, &users).await?;
    debug!(
        log = %logged.id,
        confirmed = users.len(),
        "admin confirmed user emails"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `PATCH /api/v1/pleroma/admin/users/resend_confirmation_email` —
/// Pleroma's `resend_confirmation_email`: queue the confirmation letter
/// again for the named accounts whose address is still unconfirmed. A
/// confirmed one has nothing left to confirm, and Pleroma's own
/// `send_confirmation_email` is reached only through the unconfirmed paths
/// — the flag is the filter here. Every resolved account is a log subject,
/// whether or not it was mailed.
///
/// # Errors
///
/// As [`force_password_reset`]'s, plus whatever the letter queueing
/// (`confirmation::send`) reports.
async fn resend_confirmation_email(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<NicknamesBody>,
) -> AppResult<StatusCode> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let users = admin::resolve_nicknames(&state.db, &body.0.nicknames).await?;
    let mut mailed = 0;
    for found in &users {
        if !found.email_confirmed.get() {
            confirmation::send(&state, found).await?;
            mailed += 1;
        }
    }
    let logged = moderation_log::resend_confirmation_email(&state.db, &admin.0, &users).await?;
    debug!(
        log = %logged.id,
        resolved = users.len(),
        mailed, "admin re-sent confirmation emails"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/v1/pleroma/admin/users/disable_mfa` — Pleroma's
/// `disable_mfa`: reset the named account's `mfa_settings` to the column
/// default — secret, backup codes and the enabled flag all gone — and
/// answer the nickname back as its `json(conn, nickname)`.
///
/// Pleroma logs nothing here; the ticket asks every write to leave an
/// entry, so the account's removal of its second factor is recorded as
/// `disable_mfa` (see [`moderation_log::disable_mfa`]).
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] from the
/// extractor for a body with no `nickname`; [`AppError::RouteNotFound`]
/// for a handle no account answers to; [`AppError::Database`] from the
/// writes.
async fn disable_mfa(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<NicknameBody>,
) -> AppResult<Json<String>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let found = user::get_by_nickname(&state.db, &body.0.nickname)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    mfa::save(&state.db, found.id, &mfa::MfaSettings::default()).await?;
    let logged = moderation_log::disable_mfa(&state.db, &admin.0, slice::from_ref(&found)).await?;
    debug!(
        log = %logged.id,
        id = %found.id,
        nickname = %found.nickname,
        "admin disabled a user's MFA"
    );
    Ok(Json(body.0.nickname))
}

/// Both accounts one follow body names, looked up by nickname: either miss
/// — and a remote follower, whose follow this instance cannot force — is
/// the module's 404, Pleroma's `with` chain answered whole. The followed
/// account may be remote: forcing a local account's follow of a mirror is
/// exactly what federation then has to carry.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] for either miss or a remote follower;
/// [`AppError::Database`] from the reads.
async fn resolve_pair(state: &AppState, body: &FollowBody) -> AppResult<(User, User)> {
    let follower = user::get_by_nickname(&state.db, &body.follower)
        .await?
        .filter(|found| found.local.get())
        .ok_or(AppError::RouteNotFound)?;
    let target = user::get_by_nickname(&state.db, &body.followed)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    Ok((follower, target))
}

/// `POST /api/v1/pleroma/admin/users/follow` — Pleroma's admin `follow`:
/// make `follower` follow `followed`, whatever the target's own answer
/// would be. A locked or remote target leaves the model's write `pending`;
/// the admin's act IS the answer, so the request is accepted on the spot
/// (Pleroma's `User.follow(follower, followed, :follow_accept)` does the
/// same even for a locked account). The user-facing endpoint's notification
/// and federation hook then runs with the accepted row, and the entry
/// joins the log.
///
/// Following yourself is nobody's order: the model's 422 (`"Can not follow
/// yourself"`) answers it.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] from the
/// extractor for an incomplete body or from the model for the self-follow;
/// [`AppError::RouteNotFound`] from [`resolve_pair`];
/// [`AppError::Forbidden`] from the model when a block stands between the
/// pair; [`AppError::Database`] from the writes and the log.
async fn follow_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<FollowBody>,
) -> AppResult<Json<&'static str>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let (follower, target) = resolve_pair(&state, &body.0).await?;
    let started = follow::follow(&state.db, &state.config.base_url, &follower, &target).await?;
    let accepted = if started.state == FollowState::Pending {
        follow::accept(&state.db, started.id).await?
    } else {
        started
    };
    after_follow_changed(&state, &follower, &target, Some(&accepted), true).await;
    let logged = moderation_log::follow(&state.db, &admin.0, &follower, &target).await?;
    debug!(
        log = %logged.id,
        follower = %follower.nickname,
        followed = %target.nickname,
        "admin forced a follow"
    );
    Ok(Json(OK))
}

/// `POST /api/v1/pleroma/admin/users/unfollow` — Pleroma's admin
/// `unfollow`: take `follower`'s follow of `followed` away, an accepted
/// relationship or a still-pending request alike, and run the same
/// hook and log as [`follow_users`]. Nothing to remove changes nothing and
/// still answers `ok` — the user endpoint's own idempotence.
///
/// # Errors
///
/// As [`follow_users`]'s, minus the self-follow refusal: the model is
/// content with a pair that never followed.
async fn unfollow_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<FollowBody>,
) -> AppResult<Json<&'static str>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let (follower, target) = resolve_pair(&state, &body.0).await?;
    let previous = follow::get(&state.db, follower.id, target.id).await?;
    let removed = follow::unfollow(&state.db, &follower, &target).await?;
    after_follow_changed(
        &state,
        &follower,
        &target,
        previous.as_ref().filter(|_| removed),
        false,
    )
    .await;
    let logged = moderation_log::unfollow(&state.db, &admin.0, &follower, &target).await?;
    debug!(
        log = %logged.id,
        follower = %follower.nickname,
        followed = %target.nickname,
        removed,
        "admin forced an unfollow"
    );
    Ok(Json(OK))
}

/// The normalized spelling of one submitted tag list: Pleroma's
/// `normalize_tags` (`lib/pleroma/user.ex`), which folds every tag the
/// admin submits to lowercase. The fold is what makes the `mrf_tag:`
/// switches forgiving — [`crate::federation::mrf::tag::Tag`] matches the
/// stored spelling exactly, and its six keys are spelled in lowercase.
fn normalized(tags: &[String]) -> Vec<String> {
    tags.iter().map(|tag| tag.to_lowercase()).collect()
}

/// Pleroma's `Enum.uniq((user.tags || []) ++ normalize_tags(tags))`, the
/// `User.tag` write: the stored tags keep their order and their spelling,
/// and every submitted tag the account does not already carry is appended
/// once, in the order it was stated. Adding a tag twice is therefore one
/// entry, which is the pair's idempotence.
fn merged_tags(stored: &[String], submitted: &[String]) -> Vec<String> {
    let mut merged = stored.to_vec();
    for tag in normalized(submitted) {
        if !merged.contains(&tag) {
            merged.push(tag);
        }
    }
    merged
}

/// Pleroma's `(user.tags || []) -- normalize_tags(tags)`, the `User.untag`
/// write: the stored tags that the submission does not name, order kept.
/// The comparison is exact, so a tag stored in some other spelling by a
/// hand-written row survives a lowercase removal — and no tag that is not
/// named goes anywhere, which is the pair's other idempotence.
fn kept_tags(stored: &[String], submitted: &[String]) -> Vec<String> {
    let gone = normalized(submitted);
    stored
        .iter()
        .filter(|tag| !gone.contains(*tag))
        .cloned()
        .collect()
}

/// The accounts a staff write may move: [`local_only`]'s filter over what
/// one request named, applied to the nickname-list routes (see
/// [`grant_role`]'s refusal of the same account in the singular).
fn local_only(found: Vec<User>) -> Vec<User> {
    found
        .into_iter()
        .filter(|account| account.local.get())
        .collect()
}

/// The staff group one `permission_group` segment names, and Pleroma's
/// `No such permission_group` 404 for any other text (its `right_add`,
/// `right_delete` and bulk twins all end in that fallback clause).
///
/// # Errors
///
/// [`AppError::NotFoundMessage`] with [`NO_SUCH_GROUP`].
fn group_of(text: &str) -> AppResult<admin::Role> {
    admin::Role::parse(text).ok_or_else(|| AppError::NotFoundMessage(NO_SUCH_GROUP.to_owned()))
}

/// The account one `permission_group` route names: the show's
/// nickname-or-id lookup, remote mirrors included. The read answers a
/// mirror's two flags (both false, and Pleroma's `right_get` looks no
/// further than the row), and the write looks one up precisely so that it
/// can refuse it with its own 400.
///
/// # Errors
///
/// [`AppError::RouteNotFound`] for text that names neither;
/// [`AppError::Database`] from the read.
async fn named_account(state: &AppState, raw: &str) -> AppResult<User> {
    admin::get_by_nickname_or_id(&state.db, raw)
        .await?
        .ok_or(AppError::RouteNotFound)
}

/// The role write the four permission-group routes share: the flag lands on
/// every account in `subjects`, one `grant` or `revoke` entry names them,
/// and the answer is Pleroma's `json(conn, update)` — the group's own
/// column, carrying the state it was written to.
///
/// # Errors
///
/// [`AppError::Database`] from the writes and the log.
async fn write_role(
    state: &AppState,
    actor: &User,
    subjects: &[User],
    role: admin::Role,
    granted: bool,
) -> AppResult<Json<Value>> {
    for found in subjects {
        admin::set_role(&state.db, found.id, role, granted).await?;
    }
    let logged = if granted {
        moderation_log::grant(&state.db, actor, subjects, role.as_str()).await?
    } else {
        moderation_log::revoke(&state.db, actor, subjects, role.as_str()).await?
    };
    debug!(
        log = %logged.id,
        group = role.as_str(),
        moved = subjects.len(),
        granted,
        "admin changed a staff group"
    );
    Ok(Json(json!({ role.column(): granted })))
}

/// The pool write both suggest routes share: `is_suggested` lands on every
/// named account that answers, one entry names the moved subjects, and the
/// answer is the moved accounts as admin entities, in the order the request
/// stated them.
///
/// # Errors
///
/// [`AppError::Database`] from the lookups, the writes and the log.
async fn write_suggestions(
    state: &AppState,
    actor: &User,
    nicknames: &[String],
    suggested: bool,
) -> AppResult<Json<AdminAccounts>> {
    let found = admin::resolve_nicknames(&state.db, nicknames).await?;
    let mut moved = Vec::with_capacity(found.len());
    for account in &found {
        if let Some(updated) = admin::set_suggested(&state.db, account.id, suggested).await? {
            moved.push(updated);
        }
    }
    let logged = if suggested {
        moderation_log::add_suggestion(&state.db, actor, &moved).await?
    } else {
        moderation_log::remove_suggestion(&state.db, actor, &moved).await?
    };
    debug!(
        log = %logged.id,
        moved = moved.len(),
        suggested, "admin changed the suggestion pool"
    );
    Ok(Json(AdminAccounts::from_users(&moved, &state.config)))
}

/// `PUT /api/v1/pleroma/admin/users/tag` — Pleroma's `tag_users`: every
/// account the request names gets the submitted tags appended, deduped and
/// lowercased by [`merged_tags`], and the write logs a `tag` entry naming
/// the accounts it found and the tags as they were spelled.
///
/// A remote mirror is taggable — the `mrf_tag:` switches exist to be put on
/// mirrors — and a handle nobody answers to is skipped as the other list
/// actions skip it, inside the same 204. A tag list that adds nothing still
/// leaves the row alone (no empty write, no `updated_at` churn) and still
/// logs.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] from the
/// extractor for a request that states no `nicknames` or no `tags`;
/// [`AppError::Database`] from the reads, the writes and the log.
async fn tag_users(
    State(state): State<AppState>,
    admin: AdminUser,
    params: ApiParams<TagsBody>,
) -> AppResult<StatusCode> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let subjects = admin::resolve_nicknames(&state.db, &params.0.nicknames).await?;
    for found in &subjects {
        let merged = merged_tags(&found.tags, &params.0.tags);
        if merged != found.tags {
            admin::set_tags(&state.db, found.id, &merged).await?;
        }
    }
    let logged = moderation_log::tag(&state.db, &admin.0, &subjects, &params.0.tags).await?;
    debug!(
        log = %logged.id,
        tagged = subjects.len(),
        "admin tagged users"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/v1/pleroma/admin/users/tag` — Pleroma's `untag_users`:
/// [`tag_users`]'s twin, with [`kept_tags`] doing the subtracting and the
/// entry logged as an `untag`.
///
/// # Errors
///
/// As [`tag_users`].
async fn untag_users(
    State(state): State<AppState>,
    admin: AdminUser,
    params: ApiParams<TagsBody>,
) -> AppResult<StatusCode> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let subjects = admin::resolve_nicknames(&state.db, &params.0.nicknames).await?;
    for found in &subjects {
        let kept = kept_tags(&found.tags, &params.0.tags);
        if kept != found.tags {
            admin::set_tags(&state.db, found.id, &kept).await?;
        }
    }
    let logged = moderation_log::untag(&state.db, &admin.0, &subjects, &params.0.tags).await?;
    debug!(
        log = %logged.id,
        untagged = subjects.len(),
        "admin untagged users"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/pleroma/admin/users/{nickname}/permission_group` — Pleroma's
/// `right_get`: the account's two staff flags, `is_moderator` and `is_admin`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:read` gate; [`AppError::RouteNotFound`] from
/// [`named_account`]; [`AppError::Database`] from the read.
async fn show_permission_group(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminPermissionGroup>> {
    admin.require(CLASS_READ, AREA_ACCOUNTS)?;
    let found = named_account(&state, &raw).await?;
    debug!(
        id = %found.id,
        nickname = %found.nickname,
        "admin permission group read"
    );
    Ok(Json(AdminPermissionGroup::from_user(&found)))
}

/// `GET /api/v1/pleroma/admin/users/{nickname}/permission_group/{group}` —
/// the very same object: Pleroma routes both spellings onto `right_get`,
/// which never reads the group. The segment is validated anyway, as the
/// ticket pins its 404.
///
/// # Errors
///
/// As [`show_permission_group`], plus [`group_of`]'s 404.
async fn show_permission_group_of(
    State(state): State<AppState>,
    admin: AdminUser,
    Path((raw, group)): Path<(String, String)>,
) -> AppResult<Json<AdminPermissionGroup>> {
    admin.require(CLASS_READ, AREA_ACCOUNTS)?;
    let role = group_of(&group)?;
    let found = named_account(&state, &raw).await?;
    debug!(
        id = %found.id,
        group = role.as_str(),
        "admin permission group read"
    );
    Ok(Json(AdminPermissionGroup::from_user(&found)))
}

/// `POST /api/v1/pleroma/admin/users/{nickname}/permission_group/{group}` —
/// Pleroma's `right_add`: the account joins the group, and the answer names
/// the one column that moved.
///
/// The write is the ticket's, and narrower than Pleroma's: an account this
/// instance does not own is the 400 [`REMOTE_ROLE`], because a staff role
/// here is a local row's flag and nothing else. The group is checked first,
/// so a bogus group is [`group_of`]'s 404 whatever the handle says — where
/// Pleroma's guard clause fails on the group before it ever looks the
/// account up.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::NotFoundMessage`] for a group off
/// the two, [`AppError::RouteNotFound`] for an account nobody answers to;
/// [`AppError::BadRequest`] for a mirror; [`AppError::Database`] from the
/// read, the write and the log.
async fn grant_role(
    State(state): State<AppState>,
    admin: AdminUser,
    Path((raw, group)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let role = group_of(&group)?;
    let found = named_account(&state, &raw).await?;
    if !found.local.get() {
        return Err(AppError::BadRequest(REMOTE_ROLE.to_owned()));
    }
    write_role(&state, &admin.0, slice::from_ref(&found), role, true).await
}

/// `DELETE /api/v1/pleroma/admin/users/{nickname}/permission_group/{group}`
/// — Pleroma's `right_delete`: the account leaves the group.
///
/// The account's own row comes first among the refusals, as it does in
/// Pleroma — and, like its `right_delete` clause that matches the acting
/// admin's nickname without reading the group, for either group: an admin
/// who could revoke their own `moderator` flag could still revoke
/// themselves out of staff. A mirror is the same 400 [`grant_role`] answers
/// it, and the bulk twin excludes the caller rather than refusing.
///
/// # Errors
///
/// As [`grant_role`], with [`AppError::Forbidden`] and [`SELF_REVOKE`] for
/// the admin who names their own account.
async fn revoke_role(
    State(state): State<AppState>,
    admin: AdminUser,
    Path((raw, group)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let role = group_of(&group)?;
    let found = named_account(&state, &raw).await?;
    if found.id == admin.0.id {
        return Err(AppError::Forbidden(SELF_REVOKE.to_owned()));
    }
    if !found.local.get() {
        return Err(AppError::BadRequest(REMOTE_ROLE.to_owned()));
    }
    write_role(&state, &admin.0, slice::from_ref(&found), role, false).await
}

/// `POST /api/v1/pleroma/admin/users/permission_group/{group}` — Pleroma's
/// `right_add_multiple`: every named LOCAL account joins the group, unknown
/// handles and mirrors skipped inside the same success, one `grant` entry
/// naming who moved, and the same one-column answer as the singular write.
///
/// # Errors
///
/// As [`grant_role`], plus the extractor's 422 for a request with no
/// `nicknames`.
async fn grant_role_bulk(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(group): Path<String>,
    params: ApiParams<NicknamesBody>,
) -> AppResult<Json<Value>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let role = group_of(&group)?;
    let found = admin::resolve_nicknames(&state.db, &params.0.nicknames).await?;
    write_role(&state, &admin.0, &local_only(found), role, true).await
}

/// `DELETE /api/v1/pleroma/admin/users/permission_group/{group}` — Pleroma's
/// `right_delete_multiple`, and the divergence the ticket pins: where
/// Pleroma refuses the whole request with its `admin/moderator` 403 once
/// the acting admin's own handle is on the list, this revokes from
/// everybody ELSE and leaves the caller's own group untouched. The answer is
/// the same one-column map, and the `revoke` entry names the accounts that
/// moved — the caller's row not among them.
///
/// # Errors
///
/// As [`grant_role_bulk`].
async fn revoke_role_bulk(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(group): Path<String>,
    params: ApiParams<NicknamesBody>,
) -> AppResult<Json<Value>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let role = group_of(&group)?;
    let found = admin::resolve_nicknames(&state.db, &params.0.nicknames).await?;
    let mut subjects = local_only(found);
    subjects.retain(|account| account.id != admin.0.id);
    write_role(&state, &admin.0, &subjects, role, false).await
}

/// `PATCH /api/v1/pleroma/admin/users/suggest` — Pleroma's
/// `UserController.suggest`: the named accounts join the pool the
/// `GET /api/v{1,2}/suggestions` listings answer from, and every one of
/// them comes back as an admin entity under `users`.
///
/// Pleroma flags whatever the handle names, mirror included; a mirror keeps
/// its flag out of those listings because the pool itself is local accounts
/// only (see [`crate::models::suggestion`]), not because this write refused
/// it.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] from the
/// extractor for a body with no `nicknames`; [`AppError::Database`] from the
/// lookups, the writes and the log.
async fn suggest_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiParams<NicknamesBody>,
) -> AppResult<Json<AdminAccounts>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    write_suggestions(&state, &admin.0, &body.0.nicknames, true).await
}

/// `PATCH /api/v1/pleroma/admin/users/unsuggest` — Pleroma's
/// `unsuggest`: [`suggest_users`] with the flag cleared and the entry
/// logged as a `remove_suggestion`.
///
/// # Errors
///
/// As [`suggest_users`].
async fn unsuggest_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiParams<NicknamesBody>,
) -> AppResult<Json<AdminAccounts>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    write_suggestions(&state, &admin.0, &body.0.nicknames, false).await
}

/// The body of `POST /api/v1/pleroma/admin/users`: the batch of accounts
/// the admin creates, at most [`MAX_CREATE_USERS`] of them.
#[derive(Debug, Deserialize)]
struct CreateUsersBody {
    /// The accounts to create, in the order the request states them.
    users: Vec<CreateUserItem>,
}

/// One `users` entry: the three keys the ticket pins, all required —
/// Pleroma's passwordless variant (a random password behind a minted
/// reset link) is not part of this endpoint's pinned shape.
#[derive(Deserialize)]
struct CreateUserItem {
    /// The handle to register, validated by
    /// [`create_local_user`](crate::models::user::register::create_local_user)'s
    /// registration rules.
    nickname: String,
    /// The address to store, format- and uniqueness-checked by the same.
    email: String,
    /// The password in plain text — only its hash ever reaches a row, and
    /// it never lives past this request.
    password: String,
}

/// The password is secret material and renders as `<redacted>`; everything
/// else shows.
impl fmt::Debug for CreateUserItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CreateUserItem")
            .field("nickname", &self.nickname)
            .field("email", &self.email)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// What one create item came to: the stored account, or the refusal the
/// registration changeset reported for it. The row rides boxed so the
/// refusal's text does not pay for the [`User`]'s size in every variant.
#[derive(Debug)]
enum Creation {
    /// The account the creation stored, active, confirmed and approved.
    Created(Box<User>),
    /// The refusal's message: the field map as JSON text, the encoding the
    /// user-facing registration's own 422 body carries.
    Refused(String),
}

/// `POST /api/v1/pleroma/admin/users` — Pleroma's `create`: a batch of
/// local accounts, each answered for itself. Where Pleroma wraps the batch
/// in one `Multi` transaction that rolls every account back on the first
/// conflict and answers 409, the ticket pins an always-200 per-item answer:
/// each entry is the stored account's `"created.json"` map or the 400
/// `"create_error.json"` one, and the accounts that passed stay created.
/// The created ones log one `create` entry, whatever the batch also
/// refused.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] for a batch over
/// [`MAX_CREATE_USERS`]; [`AppError::Internal`] or [`AppError::Database`]
/// from the shared registration path's server-side failures (a hashing,
/// key-generation or database fault is nobody's validation error and
/// aborts the request) and from the log.
async fn create_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<CreateUsersBody>,
) -> AppResult<Json<Vec<Value>>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    if body.0.users.len() > MAX_CREATE_USERS {
        return Err(AppError::Unprocessable(format!(
            "Cannot create more than {MAX_CREATE_USERS} users in one request"
        )));
    }
    let mut results = Vec::with_capacity(body.0.users.len());
    let mut created = Vec::new();
    let mut refused = 0;
    for item in &body.0.users {
        match create_one(&state, item).await? {
            Creation::Created(user) => {
                let user = *user;
                results.push(created_item(&user));
                created.push(user);
            }
            Creation::Refused(message) => {
                results.push(refused_item(item, &message));
                refused += 1;
            }
        }
    }
    let logged = moderation_log::create_users(&state.db, &admin.0, &created).await?;
    debug!(
        log = %logged.id,
        created = created.len(),
        refused, "admin created users"
    );
    Ok(Json(results))
}

/// One create item through the shared registration path, with the three
/// gates the admin creation bypasses: the display name and the profile
/// note stay the registration's own defaults (the handle, an empty note —
/// Pleroma's admin create likewise puts the nickname in `name`), and
/// [`provider_email_verified`](crate::models::user::register::RegisterParams::provider_email_verified)
/// stands for the confirmation bypass — the admin who set the password
/// vouches for the address, so `ACCOUNT_ACTIVATION_REQUIRED` leaves the
/// row confirmed and queues no letter (Pleroma's
/// `register_changeset(…, confirmed: true)`).
/// The approval flag, which the registration stores from the instance
/// mode whatever the caller says, is lifted afterwards by
/// [`creation_bypasses_approval`].
///
/// # Errors
///
/// As [`create_users`]'s for a server-side [`register::RegisterError`];
/// validation refusals come back as [`Creation::Refused`].
async fn create_one(state: &AppState, item: &CreateUserItem) -> AppResult<Creation> {
    let params = register::RegisterParams {
        nickname: item.nickname.clone(),
        email: Some(item.email.clone()),
        password: item.password.clone(),
        name: None,
        bio: None,
        reason: None,
        provider_email_verified: true,
    };
    match register::create_local_user(state, params, None).await {
        Ok(stored) => Ok(Creation::Created(Box::new(
            creation_bypasses_approval(state, stored).await?,
        ))),
        Err(register::RegisterError::Invalid(errors)) => {
            Ok(Creation::Refused(field_errors_message(&errors)))
        }
        Err(register::RegisterError::Failed(err)) => Err(err),
    }
}

/// The approval half of the bypass: with `REGISTRATIONS_APPROVAL_REQUIRED`
/// the freshly stored row awaits staff, and an admin-created account must
/// not — [`set_approved`] lifts the flag and hands the finished row back
/// for the answer. A row that went away mid-request keeps the copy the
/// insert returned.
///
/// [`set_approved`]: crate::models::user::admin::set_approved
async fn creation_bypasses_approval(state: &AppState, stored: User) -> AppResult<User> {
    if !stored.approval_pending.get() {
        return Ok(stored);
    }
    Ok(admin::set_approved(&state.db, stored.id)
        .await?
        .unwrap_or(stored))
}

/// The per-item `error` message: the refused field map as JSON text, the
/// same encoding [`RegisterError`] carries into the user-facing
/// registration's 422 body. The fallback fold is for a map serde could
/// never encode anyway; it reads like [`RegisterError`]'s own `Display`.
///
/// [`RegisterError`]: crate::models::user::register::RegisterError
fn field_errors_message(errors: &register::FieldErrors) -> String {
    serde_json::to_string(errors).unwrap_or_else(|_| {
        errors
            .iter()
            .map(|(field, messages)| format!("{field}: {}", messages.join(", ")))
            .collect::<Vec<String>>()
            .join("; ")
    })
}

/// The per-item success entry: Pleroma's `"created.json"` — code 200, type
/// `"success"`, the stored address and handle as `data` (the
/// `password_reset_link` Pleroma adds to its passwordless items has no
/// place in this endpoint's pinned shape).
fn created_item(user: &User) -> Value {
    json!({
        "code": 200,
        "type": "success",
        "data": { "email": user.email, "nickname": user.nickname },
    })
}

/// The per-item failure entry: Pleroma's `"create_error.json"` with the
/// ticket's 400 — code, type `"error"`, the refusal's message, and the
/// handle and address as the request submitted them.
fn refused_item(item: &CreateUserItem, message: &str) -> Value {
    json!({
        "code": 400,
        "type": "error",
        "error": message,
        "data": { "nickname": item.nickname, "email": item.email },
    })
}

/// `DELETE /api/v1/pleroma/admin/users` — Pleroma's `delete`: every named
/// account that exists starts its deletion, and the answer is the JSON
/// array of the nicknames started. Two divergences from Pleroma's
/// `do_deletes`, both the ticket's: unknown handles are skipped inside the
/// same success (where a Pleroma miss dies on the nil user's `ap_id`), and
/// the caller's own account is ignored — an admin cannot delete themselves
/// out of the instance through their own console.
///
/// The `nicknames[]` arrives in the body or the query string, bracketed or
/// bare, through [`ApiParams`].
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] from the
/// extractor for a request that names nothing; [`AppError::Database`] from
/// the lookups, the writes, the queueing and the log.
async fn delete_users(
    State(state): State<AppState>,
    admin: AdminUser,
    params: ApiParams<NicknamesBody>,
) -> AppResult<Json<Vec<String>>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let found = admin::resolve_nicknames(&state.db, &params.0.nicknames).await?;
    let subjects: Vec<User> = found
        .into_iter()
        .filter(|account| account.id != admin.0.id)
        .collect();
    for account in &subjects {
        start_deletion(&state, account).await?;
    }
    let nicknames: Vec<String> = subjects
        .iter()
        .map(|account| account.nickname.clone())
        .collect();
    let logged = moderation_log::delete(&state.db, &admin.0, &subjects).await?;
    debug!(
        log = %logged.id,
        deleted = nicknames.len(),
        "admin deleted users"
    );
    Ok(Json(nicknames))
}

/// The admin deletion's start, the same write the user-facing
/// `POST /api/pleroma/delete_account` performs: [`mark_deleted`] stamps
/// `deleted_at` and deactivates beside [`delete_all_for_user`]'s revocation
/// of every token, in one transaction, and the
/// [`users.delete`](deletion::KIND_DELETE) sweep goes onto
/// [`QUEUE_BACKGROUND`]. The `Delete` announcement to the peers is the
/// sweep's own first act, not this endpoint's — a client that gets this
/// 200 must not wait on federation.
///
/// [`mark_deleted`]: crate::models::user::mark_deleted
/// [`delete_all_for_user`]: crate::models::oauth_token::delete_all_for_user
async fn start_deletion(state: &AppState, account: &User) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    user::mark_deleted(&mut *tx, account.id).await?;
    let revoked = oauth_token::delete_all_for_user(&mut *tx, account.id).await?;
    tx.commit().await?;
    let queued = enqueue(
        &state.db,
        NewJob {
            queue: QUEUE_BACKGROUND,
            kind: deletion::KIND_DELETE,
            args: deletion::job_args(account.id),
            run_at: None,
            max_attempts: None,
            unique_key: None,
        },
    )
    .await?;
    debug!(
        account = %account.nickname,
        revoked,
        queued = queued.is_some(),
        "admin started a user deletion"
    );
    Ok(())
}

/// The activation write the activate/deactivate pair shares: `is_active`
/// lands on every named account that answers — the acting admin's own row
/// excepted when the request deactivates, the self-protection the ticket
/// pins (Pleroma protects nobody here) — and the answer is the moved
/// accounts as admin entities, in the order the request stated them.
///
/// A deactivation is the flag alone: no token is revoked (the bearer gate
/// answers a deactivated owner's requests with its own 403 for as long as
/// the rows live), no deletion is stamped and nothing federates, exactly
/// as `POST /api/pleroma/disable_account`'s note about the actor document
/// staying resolvable.
///
/// # Errors
///
/// [`AppError::Database`] from the lookups, the writes and the log.
async fn write_activation(
    state: &AppState,
    actor: &User,
    nicknames: &[String],
    active: bool,
) -> AppResult<Json<AdminAccounts>> {
    let found = admin::resolve_nicknames(&state.db, nicknames).await?;
    let mut moved = Vec::with_capacity(found.len());
    for account in &found {
        if !active && account.id == actor.id {
            continue;
        }
        if let Some(updated) = admin::set_active(&state.db, account.id, active).await? {
            moved.push(updated);
        }
    }
    let logged = if active {
        moderation_log::activate(&state.db, actor, &moved).await?
    } else {
        moderation_log::deactivate(&state.db, actor, &moved).await?
    };
    debug!(
        log = %logged.id,
        moved = moved.len(),
        active, "admin changed account activations"
    );
    Ok(Json(AdminAccounts::from_users(&moved, &state.config)))
}

/// `PATCH /api/v1/pleroma/admin/users/activate` — Pleroma's `activate`:
/// [`write_activation`] with the flag set.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] from the
/// extractor for a body with no `nicknames`; [`AppError::Database`] from
/// [`write_activation`].
async fn activate_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<NicknamesBody>,
) -> AppResult<Json<AdminAccounts>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    write_activation(&state, &admin.0, &body.0.nicknames, true).await
}

/// `PATCH /api/v1/pleroma/admin/users/deactivate` — Pleroma's
/// `deactivate`: [`write_activation`] with the flag cleared, and the
/// caller's own account skipped rather than locked out.
///
/// # Errors
///
/// As [`activate_users`].
async fn deactivate_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<NicknamesBody>,
) -> AppResult<Json<AdminAccounts>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    write_activation(&state, &admin.0, &body.0.nicknames, false).await
}

/// `PATCH /api/v1/pleroma/admin/users/{nickname}/toggle_activation` —
/// Pleroma's `toggle_activation`: flip the flag of the one account the
/// segment names (nickname or id, the show's lookup, mirror included),
/// answer its single admin entity and log the move that was made — an
/// activation logged `activate`, a deactivation `deactivate`, Pleroma's
/// own choice of action for the entry. Unlike the bulk pair, no account is
/// protected: the admin who toggles their own console off gets the entity
/// of a deactivated self, the state Pleroma's route answers too.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::RouteNotFound`] from
/// [`named_account`] or from the write matching no row;
/// [`AppError::Database`] from the write and the log.
async fn toggle_activation(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminAccount>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let found = named_account(&state, &raw).await?;
    let activate = !found.is_active.get();
    let updated = admin::set_active(&state.db, found.id, activate)
        .await?
        .ok_or(AppError::RouteNotFound)?;
    let subjects = slice::from_ref(&found);
    let logged = if activate {
        moderation_log::activate(&state.db, &admin.0, subjects).await?
    } else {
        moderation_log::deactivate(&state.db, &admin.0, subjects).await?
    };
    debug!(
        log = %logged.id,
        id = %found.id,
        nickname = %found.nickname,
        activated = activate,
        "admin toggled an account's activation"
    );
    Ok(Json(AdminAccount::from_user(&updated, &state.config)))
}

/// `PATCH /api/v1/pleroma/admin/users/approve` — Pleroma's `approve`:
/// every named account that answers is among the answered `users`, in the
/// order the request stated them, and the ones still awaiting staff get
/// their `approval_pending` cleared and the `account_approved` letter —
/// an already-approved account is content to be handed back unchanged,
/// Pleroma's `approve(%User{is_approved: true})` clause, mailed nothing
/// and written nothing. No token is created: approval opens the door, the
/// account still signs in with its own password. One `approve` entry names
/// every resolved subject, moved or not.
///
/// # Errors
///
/// As [`activate_users`].
async fn approve_users(
    State(state): State<AppState>,
    admin: AdminUser,
    body: ApiBody<NicknamesBody>,
) -> AppResult<Json<AdminAccounts>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let found = admin::resolve_nicknames(&state.db, &body.0.nicknames).await?;
    let mut answered = Vec::with_capacity(found.len());
    for account in found {
        if account.approval_pending.get() {
            answered.push(approve_one(&state, account).await?);
        } else {
            answered.push(account);
        }
    }
    let logged = moderation_log::approve(&state.db, &admin.0, &answered).await?;
    debug!(
        log = %logged.id,
        resolved = answered.len(),
        "admin approved users"
    );
    Ok(Json(AdminAccounts::from_users(&answered, &state.config)))
}

/// The write behind one approval: [`set_approved`] clears the gate and
/// hands the fresh row back (a row that went away mid-request keeps the
/// stored copy as the answer), and the newly-approved account gets its
/// letter through the mailer queue when it gave an address at all —
/// Pleroma's `send_user_approval_email` has the same guard on its
/// `is_binary(email)` clause.
///
/// [`set_approved`]: crate::models::user::admin::set_approved
async fn approve_one(state: &AppState, pending: User) -> AppResult<User> {
    let Some(updated) = admin::set_approved(&state.db, pending.id).await? else {
        return Ok(pending);
    };
    if let Some(email) = updated.email.as_deref() {
        mailer::deliver_later(state, templates::account_approved(&state.config, email)).await?;
    }
    Ok(updated)
}

/// The listing criteria one request states: the five named criteria, the
/// comma list of `filters` and the page window, read off the raw query
/// string `Uri::query` returns (percent-decoding and all).
///
/// Every parameter is optional; the page window keeps the admin listings'
/// defaults (page 1, [`super::DEFAULT_PAGE_SIZE`] rows, the
/// [`super::MAX_PAGE_SIZE`] cap), so an absent or unusable number never
/// refuses the request.
fn list_params(query: Option<&str>) -> admin::ListParams {
    let pairs = QueryPairs::parse(query);
    admin::ListParams {
        query: pairs.value("query"),
        name: pairs.value("name"),
        email: pairs.value("email"),
        tags: pairs.values("tags"),
        actor_types: pairs.values("actor_types"),
        filters: admin::ListFilters::parse(pairs.value("filters").as_deref()),
        page: super::page_number_of(pairs.value("page").as_deref(), super::DEFAULT_PAGE),
        page_size: super::page_size_of(pairs.value("page_size").as_deref()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No query string at all is the criterion-free first page at the
    /// default size.
    #[test]
    fn an_absent_query_is_the_default_page() {
        for raw in [None, Some("")] {
            let params = list_params(raw);
            assert_eq!(params.page, super::super::DEFAULT_PAGE, "the first page");
            assert_eq!(
                params.page_size,
                super::super::DEFAULT_PAGE_SIZE,
                "and Pleroma's size"
            );
            assert!(
                params.query.is_none()
                    && params.name.is_none()
                    && params.email.is_none()
                    && params.actor_types.is_empty()
                    && params.filters == admin::ListFilters::default(),
                "nothing to filter on: {params:?}"
            );
        }
    }

    /// Every criterion the request states reaches the params, in the
    /// spelling the admin frontend sends them.
    #[test]
    fn the_named_criteria_are_read_from_the_query() {
        let params = list_params(Some(
            "query=al%20ice&name=Alice&email=alice%40example.org&tags[]=bots&tags[]=ops&actor_types[]=Service&page=3&page_size=10",
        ));
        assert_eq!(params.query.as_deref(), Some("al ice"), "the decoded query");
        assert_eq!(params.name.as_deref(), Some("Alice"), "the display name");
        assert_eq!(
            params.email.as_deref(),
            Some("alice@example.org"),
            "the decoded email"
        );
        assert_eq!(params.tags, vec!["bots", "ops"], "both bracketed tags");
        assert_eq!(params.actor_types, vec!["Service"], "and the actor type");
        assert_eq!(params.page, 3, "the page number");
        assert_eq!(params.page_size, 10, "and the size");
    }

    /// An array key arrives bracketed or bare, and both spellings fill the
    /// same list in occurrence order; an empty element is no criterion.
    #[test]
    fn an_array_key_arrives_in_both_spellings() {
        let params = list_params(Some(
            "tags=a&tags[]=b&tags=&actor_types=Bot&actor_types[]=Service",
        ));
        assert_eq!(params.tags, vec!["a", "b"], "bare and bracketed together");
        assert_eq!(
            params.actor_types,
            vec!["Bot", "Service"],
            "the same for actor types"
        );
    }

    /// The `filters` comma list is handed to the model's parser whole, and
    /// the page window keeps its defaults and its cap when a parameter is
    /// no usable number.
    #[test]
    fn the_filters_and_the_window_are_read_leniently() {
        let params = list_params(Some(
            "filters=is_admin,need_approval,ghost&page=x&page_size=9000",
        ));
        assert!(
            params.filters.is_admin.get() && params.filters.need_approval.get(),
            "two known filters"
        );
        assert!(
            !params.filters.local.get() && !params.filters.active.get(),
            "and nothing else: {params:?}"
        );
        assert_eq!(
            params.page,
            super::super::DEFAULT_PAGE,
            "a non-number page is the first page"
        );
        assert_eq!(
            params.page_size,
            super::super::MAX_PAGE_SIZE,
            "and an oversized page is capped"
        );
    }

    /// A repeated scalar keeps its last value, the way a query map folds
    /// duplicates.
    #[test]
    fn a_repeated_scalar_keeps_its_last_value() {
        let params = list_params(Some("query=first&query=second"));
        assert_eq!(
            params.query.as_deref(),
            Some("second"),
            "the last occurrence wins"
        );
    }

    /// The credentials body parsed out of one JSON value, panicking on a
    /// shape the extractor would have refused.
    fn params(body: Value) -> CredentialsParams {
        serde_json::from_value::<CredentialsParams>(body).expect("the fixture body parses")
    }

    /// An absent key and a JSON `null` read alike — the changeset drops
    /// both before casting — and a form's string booleans reach the same
    /// flags as JSON's, while unusable bool text is the parse error the
    /// extractor turns into its 422.
    #[test]
    fn the_credentials_body_reads_null_as_absent_and_forms_as_flex() {
        let bare = params(json!({}));
        assert!(
            bare.name.is_none()
                && bare.email.is_none()
                && bare.password.is_none()
                && bare.is_locked.is_none()
                && !bare.any_flag(),
            "nothing stated: {bare:?}"
        );
        let nulled = params(json!({ "name": null, "pleroma_settings_store": null }));
        assert!(
            nulled.name.is_none() && nulled.pleroma_settings_store.is_none(),
            "nulls are absence"
        );
        let flagged = params(json!({
            "is_locked": "on",
            "hide_favorites": "0",
            "is_discoverable": true,
        }));
        assert_eq!(flagged.is_locked, Some(true), "the form's on");
        assert_eq!(flagged.hide_favorites, Some(false), "and its 0");
        assert_eq!(flagged.is_discoverable, Some(true), "and JSON's own");
        assert!(flagged.any_flag(), "a stated flag counts");
        let junk = serde_json::from_value::<CredentialsParams>(json!({ "is_locked": "maybe" }));
        assert!(junk.is_err(), "unusable bool text is the extractor's 422");
    }

    /// The password check reports the password's own fault and the
    /// confirmation's mismatch independently, the way a changeset does:
    /// blank, too short, mismatching and the pair clean.
    #[test]
    fn the_password_check_reports_both_sides_of_its_rule() {
        /// One run of the check against `body`, asserted to leave `expected_write`
        /// standing and report exactly `expected_errors`.
        fn check(
            body: Value,
            expected_write: Option<&str>,
            expected_errors: &register::FieldErrors,
        ) {
            let config = Config::for_tests();
            let mut errors = register::FieldErrors::new();
            let write = check_password(&config, &params(body), &mut errors);
            assert_eq!(
                write.as_deref(),
                expected_write,
                "the write the check leaves standing"
            );
            assert_eq!(&errors, expected_errors, "the refusals it reported");
        }

        let at_least = format!(
            "should be at least {} character(s)",
            Config::for_tests().min_password_length
        );

        check(
            json!({ "password": "" }),
            None,
            &register::FieldErrors::from([("password", vec![CANNOT_BE_BLANK.to_owned()])]),
        );
        check(
            json!({ "password": "short", "password_confirmation": "other" }),
            None,
            &register::FieldErrors::from([
                ("password", vec![at_least]),
                ("password_confirmation", vec![DOES_NOT_MATCH.to_owned()]),
            ]),
        );
        check(
            json!({ "password": "long enough", "password_confirmation": "nope" }),
            Some("long enough"),
            &register::FieldErrors::from([(
                "password_confirmation",
                vec![DOES_NOT_MATCH.to_owned()],
            )]),
        );
        check(
            json!({ "password": "long enough", "password_confirmation": "long enough" }),
            Some("long enough"),
            &register::FieldErrors::new(),
        );
        check(
            json!({ "password_confirmation": "stray" }),
            None,
            &register::FieldErrors::new(),
        );
    }

    /// The scope and actor-kind checks keep to their lists — and the kind
    /// re-decides `bot` exactly as the columns must agree.
    #[test]
    fn the_list_checks_refuse_what_is_not_on_their_list() {
        let mut errors = register::FieldErrors::new();
        assert_eq!(
            checked_scope(Some("private"), &mut errors),
            Some("private"),
            "an ordinary visibility passes"
        );
        assert_eq!(
            checked_scope(None, &mut errors),
            None,
            "absence keeps the stored one"
        );
        assert_eq!(
            checked_scope(Some("leaked"), &mut errors),
            None,
            "off the list"
        );

        assert_eq!(
            checked_actor(Some("Service"), &mut errors),
            Some(("Service".to_owned(), Flag::new(true))),
            "the automated kind makes a bot"
        );
        assert_eq!(
            checked_actor(Some("Person"), &mut errors),
            Some(("Person".to_owned(), Flag::new(false))),
            "and the human one unmakes it"
        );
        assert_eq!(
            checked_actor(Some("Group"), &mut errors),
            None,
            "Group is not settable"
        );
        assert_eq!(
            errors,
            register::FieldErrors::from([
                ("default_scope", vec![INCLUSION.to_owned()]),
                ("actor_type", vec![INCLUSION.to_owned()]),
            ]),
            "both refusals name the inclusion"
        );
    }

    /// The bio rule counts characters against the configured cap and
    /// answers the user endpoint's own message.
    #[test]
    fn the_note_check_mirrors_the_user_endpoint() {
        let config = Config::for_tests();
        let mut errors = register::FieldErrors::new();
        assert_eq!(
            checked_note(Some("hello"), &config, &mut errors).as_deref(),
            Some("hello"),
            "a short note waits for its rendering"
        );
        let long = "ü".repeat(config.max_bio_chars + 1);
        assert!(
            checked_note(Some(&long), &config, &mut errors).is_none(),
            "characters, not bytes"
        );
        assert_eq!(
            errors.get("bio"),
            Some(&vec![format!(
                "Bio is too long (maximum is {} characters)",
                config.max_bio_chars
            )]),
            "the user endpoint's wording, under bio"
        );
    }

    /// The settings-store reader: an object and the JSON text of one both
    /// come back as objects, junk text folds to `Value::Null`, and any
    /// other value is handed back for [`checked_store`]'s `else` arm to
    /// refuse — the fold and the let-else together gate every shape.
    #[test]
    fn the_store_reader_takes_objects_and_their_text() {
        assert_eq!(
            store_value(&json!({"a": 1})),
            json!({"a": 1}),
            "an object is its own answer"
        );
        assert_eq!(
            store_value(&json!("{\"a\": 1}")),
            json!({"a": 1}),
            "the text of one parses"
        );
        assert_eq!(
            store_value(&json!(["a"])),
            json!(["a"]),
            "an array is returned as it stands, for the object test to reject"
        );
        assert_eq!(
            store_value(&json!("junk")),
            Value::Null,
            "and junk text folds"
        );
    }

    /// Neither the credentials body nor an issued reset link ever renders
    /// its secrets.
    #[test]
    fn the_secret_bodies_redact_their_debug_output() {
        let body = params(json!({
            "password": "hunter2 password",
            "password_confirmation": "hunter2 password",
        }));
        let shown_body = format!("{body:?}");
        assert!(
            !shown_body.contains("hunter2"),
            "the plaintext never reaches a log line: {shown_body}"
        );
        let issued = PasswordResetLink::new("https://test.example", "a-token-text");
        let shown_link = format!("{issued:?}");
        assert!(
            !shown_link.contains("a-token-text"),
            "the minted credential never either: {shown_link}"
        );
        assert_eq!(
            serde_json::to_value(&issued).expect("the answer serializes"),
            json!({
                "token": "a-token-text",
                "link": "https://test.example/api/v1/pleroma/password_reset/a-token-text",
            }),
            "the wire answer carries both where Debug withholds them"
        );
    }

    /// The three list bodies read exactly the parameters the routes pin,
    /// and the follow pair insists on both names.
    #[test]
    fn the_bodies_read_their_pinned_parameters() {
        let names: NicknamesBody =
            serde_json::from_value(json!({ "nicknames": ["a", "b"] })).expect("the list body");
        assert_eq!(names.nicknames, vec!["a", "b"], "the submitted order");
        let one: NicknameBody =
            serde_json::from_value(json!({ "nickname": "alice" })).expect("the lone body");
        assert_eq!(one.nickname, "alice");
        let pair: FollowBody = serde_json::from_value(json!({
            "follower": "alice",
            "followed": "bob",
        }))
        .expect("the follow body");
        assert_eq!(pair.follower, "alice", "both ends");
        assert_eq!(pair.followed, "bob", "are required");
        assert!(
            serde_json::from_value::<FollowBody>(json!({ "follower": "alice" }))
                .expect_err("half a pair does not parse")
                .to_string()
                .contains("followed"),
            "and the error names the missing one"
        );
        assert!(
            serde_json::from_value::<NicknamesBody>(json!({}))
                .expect_err("no list does not parse")
                .to_string()
                .contains("nicknames"),
            "and says so"
        );
    }

    /// The tag pair's body reads both lists, and a request that states only
    /// one of them is the extractor's 422 (Pleroma's clause requires both).
    #[test]
    fn the_tag_body_requires_both_lists() {
        let both: TagsBody = serde_json::from_value(json!({
            "nicknames": ["alice", "bob"],
            "tags": ["staff", "mrf_tag:sandbox"],
        }))
        .expect("the tag body");
        assert_eq!(both.nicknames, vec!["alice", "bob"], "both handles");
        assert_eq!(
            both.tags,
            vec!["staff", "mrf_tag:sandbox"],
            "an MRF tag is an ordinary string here, and is allowed"
        );
        for missing in [json!({"nicknames": ["alice"]}), json!({"tags": ["x"]})] {
            assert!(
                serde_json::from_value::<TagsBody>(missing.clone()).is_err(),
                "half a request does not parse: {missing}"
            );
        }
    }

    /// One `Vec<String>` from bare text, for the tag tests.
    fn texts(items: &[&str]) -> Vec<String> {
        items.iter().map(|text| (*text).to_owned()).collect()
    }

    /// The tag fold, order and all: the append is Pleroma's
    /// `Enum.uniq(stored ++ normalize_tags(tags))` and the removal its
    /// `stored -- normalize_tags(tags)`, each over the LOWERCASED
    /// submission — which is what makes a tag the `mrf_tag:` switches
    /// actually match, and what makes both writes idempotent.
    #[test]
    fn the_tag_lists_merge_and_subtract_over_the_lowercase_submission() {
        let stored = texts(&["bots", "staff", "MANTAINED"]);
        assert_eq!(
            merged_tags(&stored, &texts(&["vip", "staff"])),
            texts(&["bots", "staff", "MANTAINED", "vip"]),
            "the stored list leads in its own spelling, the new tag follows, the held one is dropped"
        );
        assert_eq!(
            merged_tags(&stored, &texts(&["STAFF", "Staff"])),
            stored,
            "tagging a held tag — twice, in any case — changes nothing"
        );
        assert_eq!(
            merged_tags(&[], &texts(&["MRF_Tag:Sandbox"])),
            texts(&["mrf_tag:sandbox"]),
            "the switch's own spelling, whatever case the admin typed"
        );
        assert_eq!(
            kept_tags(&stored, &texts(&["maintained"])),
            stored,
            "the removal compares the stored spelling exactly, so a tag kept in caps since before this
             writer existed survives a lowercase untag — Pleroma's subtraction, quirk included"
        );
        assert_eq!(
            kept_tags(&stored, &texts(&["bots", "STAFF"])),
            texts(&["MANTAINED"]),
            "every named tag goes, the rest stays in order"
        );
        assert_eq!(
            kept_tags(&stored, &texts(&["nobody"])),
            stored,
            "an unheld tag is subtracted in peace"
        );
        assert_eq!(
            merged_tags(&stored, &[]).len() + kept_tags(&stored, &[]).len(),
            6,
            "an empty tag list writes nothing either way"
        );
    }

    /// The bulk write's subject filter is exercised end to end (a mirror in
    /// a nickname list is skipped); the group segment is the pure part:
    /// Pleroma's two names, and no other text.
    #[test]
    fn only_the_two_group_names_are_a_group() {
        assert_eq!(group_of("admin").ok(), Some(admin::Role::Admin));
        assert_eq!(group_of("moderator").ok(), Some(admin::Role::Moderator));
        for text in ["Admin", "owner", "", "is_admin"] {
            let err = group_of(text).expect_err("no such group name");
            assert_eq!(err.to_string(), NO_SUCH_GROUP, "Pleroma's wording");
        }
    }

    /// The create body reads its list of three-key items and nothing
    /// looser: every key of every item is required, and an item that
    /// states two of them is the parse error the extractor turns into
    /// its 422 — the same for a body with no list at all.
    #[test]
    fn the_create_body_requires_every_key_of_every_item() {
        let batch: CreateUsersBody = serde_json::from_value(json!({
            "users": [
                {"nickname": "alice", "email": "alice@example.org", "password": "long enough"},
                {"nickname": "bob", "email": "bob@example.org", "password": "another pass"},
            ],
        }))
        .expect("the create body");
        assert_eq!(batch.users.len(), 2, "both items");
        assert_eq!(batch.users[0].nickname, "alice", "each with");
        assert_eq!(batch.users[1].password, "another pass", "all three keys");
        for broken in [
            json!({"users": [{"nickname": "a", "email": "a@b.cx"}]}),
            json!({"users": [{"nickname": "a", "password": "p"}]}),
            json!({"users": [{"email": "a@b.cx", "password": "p"}]}),
            json!({}),
        ] {
            assert!(
                serde_json::from_value::<CreateUsersBody>(broken.clone()).is_err(),
                "an incomplete request does not parse: {broken}"
            );
        }
    }

    /// The submitted password never renders, and the per-item answers keep
    /// Pleroma's two shapes: the success entry the ticket pins its data
    /// keys to, and the refusal carrying the message and the submitted
    /// handle and address.
    #[test]
    fn the_create_items_redact_and_render_the_pinned_shapes() {
        let item: CreateUserItem = serde_json::from_value(json!({
            "nickname": "alice",
            "email": "alice@example.org",
            "password": "hunter2 password",
        }))
        .expect("one item");
        let shown = format!("{item:?}");
        assert!(
            !shown.contains("hunter2"),
            "the plaintext never reaches a log line: {shown}"
        );
        assert_eq!(
            refused_item(&item, "nope"),
            json!({
                "code": 400,
                "type": "error",
                "error": "nope",
                "data": {"nickname": "alice", "email": "alice@example.org"},
            }),
            "the refusal, with what the request submitted"
        );
    }

    /// The per-item `error` text is the field map as JSON, every refusal
    /// of every field inside it — the encoding the user-facing
    /// registration's own 422 body carries.
    #[test]
    fn the_create_refusal_encodes_the_whole_field_map() {
        let mut errors = register::FieldErrors::new();
        let first: Option<Vec<String>> =
            errors.insert("nickname", vec!["has already been taken".to_owned()]);
        let second: Option<Vec<String>> =
            errors.insert("email", vec!["has invalid format".to_owned()]);
        drop((first, second));
        assert_eq!(
            field_errors_message(&errors),
            "{\"email\":[\"has invalid format\"],\"nickname\":[\"has already been taken\"]}",
            "the map's own JSON text, keys sorted"
        );
        assert_eq!(
            field_errors_message(&register::FieldErrors::new()),
            "{}",
            "and an empty map is no special case"
        );
    }
}
