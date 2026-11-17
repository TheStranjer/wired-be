//! The Mastodon admin API's account endpoints over HTTP: three reads
//! (WBE-210) and five moderation writes (WBE-213).
//!
//! The reads are `GET /api/v1/admin/accounts`,
//! `GET /api/v1/admin/accounts/{id}` and `GET /api/v2/admin/accounts`; the
//! writes are `DELETE /api/v1/admin/accounts/{id}` and its `enable`,
//! `approve`, `reject` and `action` twins.
//!
//! The reference is `Pleroma.Web.MastodonAPI.Admin.AccountController`
//! (`lib/pleroma/web/mastodon_api/admin/controllers/account_controller.ex`),
//! whose `index`, `index2` and `show` are the three reads and whose
//! `delete`, `enable`, `approve`, `reject` and `account_action` are the five
//! writes. Pleroma routes all eight under `/api/v1/admin` (its
//! `lib/pleroma/web/router.ex`), so unlike the reads there is no v2 twin of
//! a write, and every one of them names its account by a *row id* segment —
//! the controller's `assign_account_by_id`, the `named_account` lookup here:
//! text that is no id and an id that names no row are the same 404
//! `{"error":"Record not found"}`.
//!
//! The controller runs the `admin_api` pipeline, whose gate plug is
//! `UserIsStaffPlug`, and its `OAuthScopesPlug` pair: `admin:read:accounts`
//! on the reads, `admin:write:accounts` on the writes. The reads keep
//! Pleroma's staff reading — an admin or a moderator, [`StaffUser`] — while
//! the ticket pins the five writes to [`AdminUser`], the admin role alone:
//! moderation is the sharpest edge in this API and the console that owns it
//! is an admin's. A moderator's otherwise valid token is therefore the 403
//! `"User is not an admin."` on all five. Each handler's first statement is
//! the scope half of the same pipeline: [`StaffUser::require`] with the
//! `admin:read` class, or [`AdminUser::require`] with `admin:write`, both on
//! the `accounts` area, so `admin:read` and `admin:read:accounts` read,
//! `admin:write` and `admin:write:accounts` write, and nothing else does.
//!
//! The three reads answer the same [`AdminAccount`] entity and page the same
//! way: [`PageParams`] keyset pagination over the *`users` row id* (decision 4
//! makes the `UUIDv7` text sortable, which is what turns a cursor into a plain
//! comparison) with Mastodon's admin page sizes — a hundred rows by default,
//! at most two hundred — under the `Link` header every other listing here
//! writes, its cursors keyed on the page's first and last id and the request's
//! own filters repeated, Pleroma's `build_pagination_fields` rule that the
//! shared [`link_query`] already implements.
//!
//! What differs between the two versions is how a request states its
//! criteria. v1's are booleans and v2's are words, and [`admin::Criterion`] is
//! the reading they share:
//!
//! - `local` and `remote` (v1) weigh [`local`](User::local); a `false` asks
//!   for the other side, which is Pleroma's `maybe_filter_local` /
//!   `maybe_filter_external` pair, and `origin=local` / `origin=remote` (v2)
//!   ask for the same two sets by name.
//! - `active` (v1) and `status=active` (v2) want an account active, approved
//!   *and* confirmed; `pending` / `status=pending` want a stated
//!   [`approval_pending`](User::approval_pending); `disabled` /
//!   `status=disabled` want [`is_active`](User::is_active) off.
//! - `staff` (v1) and `permissions=staff` (v2) want an admin or a moderator.
//! - `username`, `display_name`, `by_domain` and `email` are the four criteria
//!   both versions state identically: the handle and the display name as
//!   case-insensitive substrings, the mirrored host and the address exactly.
//! - `silenced`, `suspended`, `sensitized`, `ip` and `invited_by` are accepted
//!   and ignored. This schema has no silence, suspension, sensitive-flag,
//!   login-address or invitation column to weigh them against, so they filter
//!   nothing instead of refusing the request — the same treatment Pleroma's
//!   controller gives a criterion its `build_criteria` never puts in.
//!
//! The instance actor is in neither listing: [`admin::mastodon_list`] ends its
//! predicate with
//! [`NOT_INSTANCE_ACTOR`](crate::models::user::NOT_INSTANCE_ACTOR) like every
//! other account listing here, while `show` names one row directly and so
//! finds the actor too, exactly as [`admin::get_by_nickname_or_id`] does.
//! The writes name a row the same way and so reach it as well: nothing here
//! hides an account from an admin who asks for it by id.
//!
//! The five writes, each answering [`AdminAccount`] — except the umbrella,
//! which answers the empty object — and each leaving a [`moderation_log`]
//! entry behind:
//!
//! - `DELETE …/accounts/{id}` starts the deletion through the very write the
//!   Pleroma console's `DELETE /users` runs,
//!   [`helpers::start_deletion`]: `deleted_at` stamped, every token revoked
//!   and the `users.delete` sweep queued. It answers the entity *as the row
//!   was*, before any of that: the client asked what it deleted, and the row
//!   it describes is on its way out. Pleroma's `delete` runs no such check
//!   and would announce the admin's own `Delete` to the peers; the ticket
//!   pins the guard, so naming your own account is the 403 `"This action is
//!   not allowed"`.
//! - `POST …/accounts/{id}/enable` is [`admin::set_active`] with the flag
//!   set: the flag alone, minting no credential — the deactivation revoked
//!   none either, so the account's own token works again the moment it is
//!   enabled — and it logs Pleroma's `enable` entry, the `activate` one.
//!   Enabling an account that was never disabled changes nothing, answers
//!   200 and logs.
//! - `POST …/accounts/{id}/approve` is the console's `PATCH /users/approve`
//!   for one account by id, sharing its write and its `account_approved`
//!   letter through [`helpers::approve_account`]. An account nobody left
//!   pending answers unchanged and is mailed nothing — Pleroma's
//!   `approve(%User{is_approved: true})` clause — and one `approve` entry
//!   names it either way.
//! - `POST …/accounts/{id}/reject` is that gate's other half: it DELETES the
//!   pending account ([`admin::delete_user`], Pleroma's `User.reject` →
//!   `User.delete` → `maybe_delete_from_db` arm, which drops the row of an
//!   account that never finished registering rather than only purging it)
//!   and posts its `account_rejected` letter to the address on the row.
//!   The answer is the entity of the row as it was read — there is no row
//!   left to render — and the `reject` entry names the account the log can
//!   no longer join to. Any other account is the 403 `"This action is not
//!   allowed"`: Pleroma's `User.reject` answers its approved arm with
//!   `{:error, "User is approved"}` and its controller turns that into a
//!   400, which the ticket replaces with the same refusal the delete route
//!   gives.
//! - `POST …/accounts/{id}/action` is Mastodon's moderation umbrella.
//!   `disable` clears `is_active`; `suspend` clears it too and queues the
//!   `users.delete` sweep — the account's CONTENT, with no `deleted_at`
//!   stamp and no token revoked, so the row stays and stays resolvable;
//!   `none`, `sensitive` and `silence` change nothing this schema can
//!   change, because there is no sensitive flag and no silence state to set
//!   (the same absence the reads answer with their `sensitive`-less entity
//!   and their `silenced: false`) and exist in the log alone. Whatever the
//!   type, the request records one entry under its own name — Pleroma's
//!   fall-through sentence, `@admin performed action disable` — and the
//!   writes that moved something record that too: a `deactivate` entry
//!   beside a `disable` or a `suspend`, exactly the entry every other
//!   writer of that flag in this repo leaves, and a `report_update` entry
//!   for the `report_id` it resolves (with the `report.updated` event every
//!   other report state change emits). Pleroma's `associated_report` guard
//!   comes with it: an id that names no report is the same 404 the account
//!   half gives, and a report about somebody else the 400 `"Report does not
//!   target this account"`. `text`, `warning_preset_id` and
//!   `send_email_notification` are accepted and ignored — this instance has
//!   no moderation warning to attach them to and Pleroma's controller reads
//!   none of them, so no field of the request's `ActionParams` names them and
//!   the body
//!   parser drops the keys. The answer is the empty object `{}`, where
//!   Pleroma answers `204` and the ticket pins `200`. A `type` off the five
//!   words is the 422 `"Validation failed: Type is invalid"` Pleroma's
//!   `CastAndValidate` plug answers a value outside the enum with, and no
//!   `type` at all the 422 `"Validation failed: Type is required"` its
//!   `required: [:type]` asks for.
//!
//! [`StaffUser`]: crate::auth::admin::StaffUser
//! [`StaffUser::require`]: crate::auth::admin::StaffUser::require
//! [`AdminUser`]: crate::auth::admin::AdminUser
//! [`AdminUser::require`]: crate::auth::admin::AdminUser::require
//! [`AdminAccount`]: crate::entities::mastodon_admin::AdminAccount
//! [`moderation_log`]: crate::models::moderation_log
//! [`helpers::start_deletion`]: crate::api::helpers::start_deletion
//! [`helpers::approve_account`]: crate::api::helpers::approve_account
//! [`admin::set_active`]: crate::models::user::admin::set_active
//! [`admin::delete_user`]: crate::models::user::admin::delete_user
//! [`User`]: crate::models::user::User

use std::slice;

use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, Uri},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::debug;

use crate::{
    api::helpers::{self, QueryPairs, link_query},
    auth::admin::{AdminUser, StaffUser},
    entities::mastodon_admin::AdminAccount,
    error::{AppError, AppResult},
    extract::ApiBody,
    jobs::{QUEUE_BACKGROUND, deletion},
    mailer::{self, templates},
    models::{
        job::{NewJob, enqueue},
        moderation_log,
        report::{self, ReportState},
        user::{
            self, User,
            admin::{self, Criterion, MastodonListParams},
        },
    },
    pagination::{PageParams, link_header, set_link_header},
    services::webhooks,
    state::AppState,
    util::id::parse_id,
};

/// The read endpoint class of [`StaffUser::require`].
const CLASS_READ: &str = "read";
/// The write endpoint class of [`AdminUser::require`]: every one of the five
/// moderation actions is gated by it, the three reads by [`CLASS_READ`]
/// (Pleroma's `OAuthScopesPlug` pair on this controller).
const CLASS_WRITE: &str = "write";
/// The admin scope area these endpoints live in
/// (`admin:read:accounts` / `admin:write:accounts`, Pleroma's
/// `OAuthScopesPlug` grants for the eight actions).
const AREA_ACCOUNTS: &str = "accounts";
/// Default page size when `limit` is absent or unusable: Mastodon's admin
/// listing default, where Pleroma's global [`PageParams`] default is 20.
const DEFAULT_LIMIT: i64 = 100;
/// Page size cap: the bound the ticket pins for these two listings.
const MAX_LIMIT: i64 = 200;
/// The `origin` value that asks for this instance's own accounts.
const ORIGIN_LOCAL: &str = "local";
/// The `origin` value that asks for the mirrors.
const ORIGIN_REMOTE: &str = "remote";
/// The `status` value that weighs active, approved and confirmed at once.
const STATUS_ACTIVE: &str = "active";
/// The `status` value that weighs a pending approval.
const STATUS_PENDING: &str = "pending";
/// The `status` value that weighs a deactivated account.
const STATUS_DISABLED: &str = "disabled";
/// The `permissions` value that weighs the two staff flags.
const PERMISSIONS_STAFF: &str = "staff";
/// The 403 body of the two refusals this surface answers alike: the admin
/// who names their own account in a deletion, and the rejection of an
/// account nobody had left pending. Pleroma reaches neither — its `delete`
/// runs no self check and its `User.reject` answers an approved account with
/// `{:error, "User is approved"}` and a 400 — the ticket pins this one
/// wording for both.
const NOT_ALLOWED: &str = "This action is not allowed";
/// The 422 body of a `type` word outside the five Mastodon spells: what
/// Pleroma's `ApiSpec.CastAndValidate` plug answers a value outside the
/// operation's enum with.
const INVALID_TYPE: &str = "Validation failed: Type is invalid";
/// The 422 body of a request that states no `type` at all — the
/// `required: [:type]` of Pleroma's `account_action_operation`, in the same
/// `Validation failed:` shape.
const MISSING_TYPE: &str = "Validation failed: Type is required";
/// The 400 body of a `report_id` whose report is about somebody else:
/// Pleroma's `associated_report` guard, which refuses to close a report
/// through an account it names no part of.
const WRONG_TARGET: &str = "Report does not target this account";

/// The `/admin/accounts` routes of the Mastodon admin API v1, nested at
/// `/admin` by [`crate::api::v1`]: the listing, its show twin, and the five
/// moderation writes the same `{id}` segment is keyed on.
///
/// The three action suffixes are static segments beside the parameter
/// `/accounts/{id}` the read occupies; axum prefers the longer static match,
/// so `POST /accounts/{id}/enable` reaches the enable handler while
/// `GET /accounts/{id}` still reaches the show one — the same pair of
/// shapes the Pleroma admin console's `/users` routes hang off each other
/// (see [`crate::api::admin::users`]).
pub fn v1_router() -> Router<AppState> {
    Router::new()
        .route("/accounts", get(list_accounts))
        .route("/accounts/{id}", get(show_account).delete(delete_account))
        .route("/accounts/{id}/enable", post(enable_account))
        .route("/accounts/{id}/approve", post(approve_account))
        .route("/accounts/{id}/reject", post(reject_account))
        .route("/accounts/{id}/action", post(account_action))
}

/// The same listing's v2 twin, `/admin/accounts` nested at `/admin` by
/// [`crate::api::v2`].
///
/// Mastodon routes no v2 show: the v1 `show_account` above is the one route
/// every client uses, whichever version it listed through.
pub fn v2_router() -> Router<AppState> {
    Router::new().route("/accounts", get(list_accounts_v2))
}

/// `GET /api/v1/admin/accounts` — Pleroma's `index`: the page of accounts the
/// request's boolean and text criteria select.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:read` gate; [`AppError::BadRequest`] for a malformed
/// pagination cursor; [`AppError::Database`] from the listing read; and
/// [`set_link_header`] reports a `Link` value that is not header-safe as
/// [`AppError::Internal`].
async fn list_accounts(
    State(state): State<AppState>,
    staff: StaffUser,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<AdminAccount>>)> {
    staff.require(CLASS_READ, AREA_ACCOUNTS)?;
    page_response(&state, &uri, v1_params(uri.query())).await
}

/// `GET /api/v2/admin/accounts` — Pleroma's `index2`: the same entities on the
/// same page, its criteria spelled as `origin`, `status` and `permissions`
/// words.
///
/// # Errors
///
/// Exactly those of [`list_accounts`].
async fn list_accounts_v2(
    State(state): State<AppState>,
    staff: StaffUser,
    OriginalUri(uri): OriginalUri,
) -> AppResult<(HeaderMap, Json<Vec<AdminAccount>>)> {
    staff.require(CLASS_READ, AREA_ACCOUNTS)?;
    page_response(&state, &uri, v2_params(uri.query())).await
}

/// `GET /api/v1/admin/accounts/{id}` — Pleroma's `show`: the one entity the id
/// names, a deactivated account and a mirror included.
///
/// `{id}` is a row id and nothing else (Pleroma's `assign_account_by_id` reads
/// it the same way): text that is no id names no account, so a malformed id
/// and an unknown one are the same 404 `{"error":"Record not found"}`.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`StaffUser`]
/// and its `admin:read` gate; [`AppError::NotFound`] as described;
/// [`AppError::Database`] from the lookup.
async fn show_account(
    State(state): State<AppState>,
    staff: StaffUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminAccount>> {
    staff.require(CLASS_READ, AREA_ACCOUNTS)?;
    let account = named_account(&state, &raw).await?;
    debug!(
        account = %account.id,
        "admin account shown"
    );
    Ok(Json(AdminAccount::from_user(&account, &state.config)))
}

/// The account one `/admin/accounts/{id}` route names, by row id and nothing
/// else — [`show_account`]'s lookup, which all five writes share.
///
/// No visibility rule on it: an admin moderates a deactivated account and
/// the instance actor exactly as an active one's, which is what
/// [`helpers::account_or_404`](crate::api::helpers::account_or_404) refuses
/// to do for the client API.
///
/// # Errors
///
/// [`AppError::NotFound`] for text that is no id and for an id that names no
/// row; [`AppError::Database`] from the read.
async fn named_account(state: &AppState, raw: &str) -> AppResult<User> {
    let Some(id) = parse_id(raw) else {
        return Err(AppError::NotFound);
    };
    user::get_by_id(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)
}

/// `DELETE /api/v1/admin/accounts/{id}` — Pleroma's `delete`: the account's
/// deletion, started the way every other admin deletion here is started, and
/// answered with the entity as the row stood when the request named it.
///
/// The self check is the ticket's: Pleroma's `delete` pipes through
/// `require_privileged_role_users_delete` and then announces whatever
/// account the path named, the caller's own included.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate, [`AppError::Forbidden`] with
/// [`NOT_ALLOWED`] for the admin's own account; [`AppError::NotFound`] from
/// [`named_account`]; [`AppError::Database`] from the writes and the log.
async fn delete_account(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminAccount>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let account = named_account(&state, &raw).await?;
    if account.id == admin.0.id {
        return Err(AppError::Forbidden(NOT_ALLOWED.to_owned()));
    }
    helpers::start_deletion(&state, &account).await?;
    let logged = moderation_log::delete(&state.db, &admin.0, slice::from_ref(&account)).await?;
    debug!(
        log = %logged.id,
        account = %account.id,
        "admin deleted an account"
    );
    Ok(Json(AdminAccount::from_user(&account, &state.config)))
}

/// `POST /api/v1/admin/accounts/{id}/enable` — Pleroma's `enable`:
/// [`admin::set_active`] with the flag set, the fresh row's entity as the
/// answer and the `activate` entry Pleroma's own action writes.
///
/// The flag is the whole gate: the deactivation revoked no token and stamped
/// no deletion, so the account's own credential works again on this answer
/// and its actor document never stopped resolving. Re-enabling an active
/// account writes the same `true` back and answers the same entity.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::NotFound`] from
/// [`named_account`] or from the write matching no row;
/// [`AppError::Database`] from the write and the log.
async fn enable_account(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminAccount>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let account = named_account(&state, &raw).await?;
    let updated = admin::set_active(&state.db, account.id, true)
        .await?
        .ok_or(AppError::NotFound)?;
    let logged = moderation_log::activate(&state.db, &admin.0, slice::from_ref(&account)).await?;
    debug!(
        log = %logged.id,
        account = %account.id,
        "admin enabled an account"
    );
    Ok(Json(AdminAccount::from_user(&updated, &state.config)))
}

/// `POST /api/v1/admin/accounts/{id}/approve` — Pleroma's `approve`: the
/// console's batch approval for one account by id, its write and its
/// `account_approved` letter shared through [`helpers::approve_account`].
///
/// An account nobody left pending is Pleroma's
/// `approve(%User{is_approved: true})` arm: handed back unchanged, mailed
/// nothing, written nothing — and still the answer's entity and still a log
/// subject. The approval mints no credential; the account signs in with the
/// password it registered.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::NotFound`] from
/// [`named_account`]; [`AppError::Database`] from the write, the letter's
/// queueing and the log.
async fn approve_account(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminAccount>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let account = named_account(&state, &raw).await?;
    let answered = if account.approval_pending.get() {
        helpers::approve_account(&state, account).await?
    } else {
        account
    };
    let logged = moderation_log::approve(&state.db, &admin.0, slice::from_ref(&answered)).await?;
    debug!(
        log = %logged.id,
        account = %answered.id,
        "admin approved an account"
    );
    Ok(Json(AdminAccount::from_user(&answered, &state.config)))
}

/// `POST /api/v1/admin/accounts/{id}/reject` — Pleroma's `reject`: the
/// pending account's row deleted outright (Pleroma's `User.reject` runs
/// `User.delete`, whose `maybe_delete_from_db` arm drops the row of an
/// account that never finished approving rather than only purging it) and
/// its `account_rejected` letter posted to the address on that row.
///
/// The entity answers as the row was read: the deletion took the row with
/// it, and `approved: false` is the state the admin just refused. The log
/// entry, written after the delete, is what survives of the account — its
/// subject columns carry no key, so naming a gone row is what they are for.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate, [`AppError::Forbidden`] with [`NOT_ALLOWED`]
/// for an account that was not pending; [`AppError::NotFound`] from
/// [`named_account`] or from the write matching no row;
/// [`AppError::Database`] from the delete, the letter's queueing and the log.
async fn reject_account(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
) -> AppResult<Json<AdminAccount>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let account = named_account(&state, &raw).await?;
    if !account.approval_pending.get() {
        return Err(AppError::Forbidden(NOT_ALLOWED.to_owned()));
    }
    if !admin::delete_user(&state.db, account.id).await? {
        return Err(AppError::NotFound);
    }
    if let Some(email) = account.email.as_deref() {
        mailer::deliver_later(&state, templates::account_rejected(&state.config, email)).await?;
    }
    let logged = moderation_log::reject(&state.db, &admin.0, slice::from_ref(&account)).await?;
    debug!(
        log = %logged.id,
        account = %account.id,
        nickname = %account.nickname,
        "admin rejected an account"
    );
    Ok(Json(AdminAccount::from_user(&account, &state.config)))
}

/// `POST /api/v1/admin/accounts/{id}/action` — Pleroma's `account_action`,
/// Mastodon's moderation umbrella: one submitted type, applied as this
/// schema can apply it and recorded whatever the answer.
///
/// The order is Pleroma's `with` chain: the type is validated before the
/// report is looked up and before anything moves, so a request naming an
/// unknown type changes nothing at all. The answer is the empty object
/// where Pleroma answers `204` — the ticket's pinning — and the moderation
/// log is what makes the request traceable.
///
/// # Errors
///
/// [`AppError::Unauthorized`] / [`AppError::Forbidden`] from [`AdminUser`]
/// and its `admin:write` gate; [`AppError::Unprocessable`] with
/// [`INVALID_TYPE`] / [`MISSING_TYPE`] from [`AccountAction::parse`];
/// [`AppError::NotFound`] from [`named_account`] or [`resolve_report`];
/// [`AppError::BadRequest`] with [`WRONG_TARGET`];
/// [`AppError::Unprocessable`] from the body extractor for a `type` that is
/// no text; and [`AppError::Database`] from the writes, the queueing and the
/// log.
async fn account_action(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(raw): Path<String>,
    ApiBody(params): ApiBody<ActionParams>,
) -> AppResult<Json<Value>> {
    admin.require(CLASS_WRITE, AREA_ACCOUNTS)?;
    let account = named_account(&state, &raw).await?;
    let action = AccountAction::parse(params.kind.as_deref())?;
    resolve_report(&state, &admin.0, &account, params.report_id.as_deref()).await?;
    apply_action(&state, &admin.0, &account, action).await?;
    Ok(Json(json!({})))
}

/// The effect of one action type: the flag its two real actions move, each
/// with the `deactivate` entry every other writer of that flag leaves, the
/// suspension's content sweep, and the entry naming the submitted type that
/// records the request itself.
///
/// # Errors
///
/// [`AppError::NotFound`] when the activation write matches no row (the
/// account went away mid-request); [`AppError::Database`] from the writes,
/// the queueing and the log.
async fn apply_action(
    state: &AppState,
    actor: &User,
    account: &User,
    action: AccountAction,
) -> AppResult<()> {
    if action.deactivates() {
        let moved = admin::set_active(&state.db, account.id, false)
            .await?
            .ok_or(AppError::NotFound)?;
        let logged = moderation_log::deactivate(&state.db, actor, slice::from_ref(&moved)).await?;
        debug!(
            log = %logged.id,
            account = %moved.id,
            "admin action deactivated an account"
        );
    }
    if action == AccountAction::Suspend {
        queue_content_deletion(state, account).await?;
    }
    let logged = moderation_log::account_action(&state.db, actor, account, action.as_str()).await?;
    debug!(
        log = %logged.id,
        account = %account.id,
        action = action.as_str(),
        "admin action recorded"
    );
    Ok(())
}

/// Queues the `users.delete` sweep a suspension asks for: the same job
/// [`helpers::start_deletion`] puts behind a deletion, and queued here on its
/// own. A suspension takes the account's CONTENT away and leaves the row
/// behind, deactivated and resolvable, so nothing is stamped in
/// `deleted_at` and no token is revoked here — the sweep's own first act is
/// the `Delete` announcement, which is what Mastodon's suspension of a local
/// account amounts to.
///
/// # Errors
///
/// [`AppError::Database`] from the queue write.
async fn queue_content_deletion(state: &AppState, account: &User) -> AppResult<()> {
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
        account = %account.id,
        queued = queued.is_some(),
        "admin queued an account's content deletion"
    );
    Ok(())
}

/// Pleroma's `associated_report` and `resolve_report` pair for the umbrella
/// request: no `report_id` is no report to resolve, an id that names none is
/// the same 404 the account half gives, a report about another account is
/// the 400 [`WRONG_TARGET`], and a report that passes both is moved to
/// `resolved` — the write, its `report_update` entry and its
/// `report.updated` event, exactly what the console's own state change does.
///
/// # Errors
///
/// [`AppError::NotFound`] for an unusable or unknown id, and for a row that
/// went between the two reads; [`AppError::BadRequest`] with
/// [`WRONG_TARGET`]; [`AppError::Database`] from the reads, the write and
/// the log.
async fn resolve_report(
    state: &AppState,
    actor: &User,
    account: &User,
    raw: Option<&str>,
) -> AppResult<()> {
    let Some(text) = raw else {
        return Ok(());
    };
    let Some(id) = parse_id(text) else {
        return Err(AppError::NotFound);
    };
    let filed = report::get(&state.db, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if filed.target_id != account.id {
        return Err(AppError::BadRequest(WRONG_TARGET.to_owned()));
    }
    let resolved = report::set_state(&state.db, id, ReportState::Resolved, Some(actor.id))
        .await?
        .ok_or(AppError::NotFound)?;
    let logged =
        moderation_log::report_update(&state.db, actor, id, resolved.state.as_str(), Some(account))
            .await?;
    webhooks::report_updated(state, &resolved).await;
    debug!(
        log = %logged.id,
        report = %id,
        state = resolved.state.as_str(),
        "admin action resolved a report"
    );
    Ok(())
}

/// The body of `POST /api/v1/admin/accounts/{id}/action`: the `type` the
/// action is named by, and the one report it resolves on the way.
///
/// Mastodon sends three more keys — `text` (the warning letter's body),
/// `warning_preset_id` and `send_email_notification` — and no field names
/// them, because nothing here reads them: this instance has no moderation
/// warning to write a letter for and Pleroma's controller — whose
/// `CastAndValidate` schema lists only `type` and `report_id` — reads none
/// either. The body parser drops the unclaimed keys, so a client sending
/// the full set is answered exactly as one sending the two that matter.
#[derive(Debug, Deserialize)]
struct ActionParams {
    /// Mastodon's `type`: one of the five [`AccountAction`] words, and
    /// required — but read as an [`Option`] so that its absence is this
    /// endpoint's [`MISSING_TYPE`] 422 rather than the extractor's generic
    /// "Invalid parameters" one.
    #[serde(rename = "type", default)]
    kind: Option<String>,
    /// The report to resolve beside the action: this schema's `UUIDv7` text,
    /// not Mastodon's integer, and `null` or absent for none.
    #[serde(default)]
    report_id: Option<String>,
}

/// The moderation type one umbrella request states.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AccountAction {
    /// `none`: nothing to do but record that the admin looked.
    NoAction,
    /// `sensitive`: Mastodon's "mark this account's media sensitive". No
    /// column holds such a state here, so the entry is all of it.
    Sensitive,
    /// `silence`: Mastodon's limit-the-reach state. This schema has no
    /// silence to model — the reads answer `silenced: false` on every row —
    /// so the entry is all of it, documented as the deliberate no-op.
    Silence,
    /// `disable`: [`is_active`](User::is_active) off, nothing else.
    Disable,
    /// `suspend`: the flag off and the account's content swept by a queued
    /// `users.delete`.
    Suspend,
}

impl AccountAction {
    /// The word Mastodon spells this action with, and the action name the
    /// moderation-log entry records it under.
    const fn as_str(self) -> &'static str {
        match self {
            Self::NoAction => "none",
            Self::Sensitive => "sensitive",
            Self::Disable => "disable",
            Self::Silence => "silence",
            Self::Suspend => "suspend",
        }
    }

    /// Whether this action is one of the two that deactivate the account.
    const fn deactivates(self) -> bool {
        matches!(self, Self::Disable | Self::Suspend)
    }

    /// Reads the submitted `type`: [`MISSING_TYPE`] for a request that
    /// states none (Pleroma's operation requires the key) and
    /// [`INVALID_TYPE`] for a word off the five (its `CastAndValidate` plug
    /// refuses anything outside the enum). The words are the enum's own, so
    /// `Disable` is no action.
    ///
    /// # Errors
    ///
    /// [`AppError::Unprocessable`] with either pinned message.
    fn parse(text: Option<&str>) -> AppResult<Self> {
        match text {
            None => Err(AppError::Unprocessable(MISSING_TYPE.to_owned())),
            Some("none") => Ok(Self::NoAction),
            Some("sensitive") => Ok(Self::Sensitive),
            Some("disable") => Ok(Self::Disable),
            Some("silence") => Ok(Self::Silence),
            Some("suspend") => Ok(Self::Suspend),
            Some(_) => Err(AppError::Unprocessable(INVALID_TYPE.to_owned())),
        }
    }
}

/// The shared answer of the two listings: one fetched page of
/// [`AdminAccount`] entities under the `Link` header its cursors belong to.
///
/// # Errors
///
/// [`AppError::BadRequest`] for a malformed cursor, [`AppError::Database`]
/// from the read and [`AppError::Internal`] from a `Link` value that is not
/// header-safe.
async fn page_response(
    state: &AppState,
    uri: &Uri,
    params: MastodonListParams,
) -> AppResult<(HeaderMap, Json<Vec<AdminAccount>>)> {
    let page = PageParams::from_query_with_default(uri.query(), DEFAULT_LIMIT, MAX_LIMIT)?;
    let rows = admin::mastodon_list(&state.db, &params, page).await?;
    debug!(listed = rows.len(), "admin accounts listed");
    let link = page_link(state, uri, &rows);
    let mut headers = HeaderMap::new();
    set_link_header(&mut headers, link.as_deref())?;
    let accounts = rows
        .iter()
        .map(|row| AdminAccount::from_user(row, &state.config))
        .collect();
    Ok((headers, Json(accounts)))
}

/// The `Link` header value for one fetched page: [`link_header`] over the
/// first (newest) and last (oldest) row's id — Pleroma's `add_link_headers`
/// cursors, keyed on the account rows this listing pages through — with the
/// request's own filters repeated, and `None` for an empty page.
fn page_link(state: &AppState, uri: &Uri, rows: &[User]) -> Option<String> {
    let newest = rows.first()?.id.to_string();
    let oldest = rows.last()?.id.to_string();
    link_header(
        &state.config.base_url,
        uri.path(),
        &link_query(uri.query()),
        &newest,
        &oldest,
    )
}

/// v1's criteria: the six booleans, each read by [`Criterion::parse`] so that
/// a `false` asks for the opposite set, and the four text criteria.
fn v1_params(query: Option<&str>) -> MastodonListParams {
    let pairs = QueryPairs::parse(query);
    MastodonListParams {
        local: Criterion::parse(pairs.value("local").as_deref()),
        remote: Criterion::parse(pairs.value("remote").as_deref()),
        active: Criterion::parse(pairs.value("active").as_deref()),
        pending: Criterion::parse(pairs.value("pending").as_deref()),
        disabled: Criterion::parse(pairs.value("disabled").as_deref()),
        staff: Criterion::parse(pairs.value("staff").as_deref()),
        ..text_params(&pairs)
    }
}

/// v2's criteria: the same six facts spelled as three word parameters —
/// `origin`, `status` and `permissions` — and the four text criteria. The two
/// `status` words with no column behind them (`silenced` and `suspended`) fall
/// through [`word_criterion`] without matching and so filter nothing, which is
/// how the ticket has them treated.
fn v2_params(query: Option<&str>) -> MastodonListParams {
    let pairs = QueryPairs::parse(query);
    let origin = pairs.value("origin");
    let status = pairs.value("status");
    let permissions = pairs.value("permissions");
    MastodonListParams {
        local: word_criterion(origin.as_deref(), ORIGIN_LOCAL),
        remote: word_criterion(origin.as_deref(), ORIGIN_REMOTE),
        active: word_criterion(status.as_deref(), STATUS_ACTIVE),
        pending: word_criterion(status.as_deref(), STATUS_PENDING),
        disabled: word_criterion(status.as_deref(), STATUS_DISABLED),
        staff: word_criterion(permissions.as_deref(), PERMISSIONS_STAFF),
        ..text_params(&pairs)
    }
}

/// The four text criteria both versions state identically, and no boolean
/// criterion at all: the caller fills those six in.
fn text_params(pairs: &QueryPairs) -> MastodonListParams {
    MastodonListParams {
        username: pairs.value("username"),
        display_name: pairs.value("display_name"),
        domain: pairs.value("by_domain"),
        email: pairs.value("email"),
        ..MastodonListParams::default()
    }
}

/// The criterion one of v2's word-valued parameters states:
/// [`Only`](Criterion::Only) when the request's value is exactly `wanted` —
/// Pleroma's `%{status: "active"}` clauses match their literals, so the words
/// are case-sensitive and `origin=Local` asks for nothing — and
/// [`Any`](Criterion::Any) for anything else: absent, empty, or a word this
/// listing does not weigh.
fn word_criterion(text: Option<&str>, wanted: &str) -> Criterion {
    if text == Some(wanted) {
        Criterion::Only
    } else {
        Criterion::Any
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The criteria a request states through no parameter at all: nothing is
    /// filtered, on either axis, in either version.
    #[test]
    fn an_absent_query_states_no_criterion() {
        for raw in [None, Some("")] {
            for params in [v1_params(raw), v2_params(raw)] {
                assert_eq!(params.local, Criterion::Any, "no origin filter");
                assert_eq!(params.remote, Criterion::Any, "and its opposite");
                assert_eq!(params.active, Criterion::Any, "no state filter");
                assert_eq!(params.pending, Criterion::Any, "no approval filter");
                assert_eq!(params.disabled, Criterion::Any, "no activation filter");
                assert_eq!(params.staff, Criterion::Any, "no role filter");
                assert!(
                    params.username.is_none()
                        && params.display_name.is_none()
                        && params.domain.is_none()
                        && params.email.is_none(),
                    "and no text criterion"
                );
            }
        }
    }

    /// v1's booleans, read in both directions: `true` weighs its own columns
    /// and `false` weighs their negation — Pleroma's
    /// `maybe_filter_local(…, %{local: false})` arm and its `active: false`
    /// clause — while an unknown spelling flips nothing.
    #[test]
    fn v1_reads_its_booleans_in_both_directions() {
        let only = v1_params(Some(
            "local=true&active=true&pending=true&disabled=true&staff=true",
        ));
        assert_eq!(only.local, Criterion::Only, "local=true is the locals");
        assert_eq!(
            only.active,
            Criterion::Only,
            "active=true is the fully green set"
        );
        assert_eq!(only.pending, Criterion::Only, "pending=true is the queue");
        assert_eq!(
            only.disabled,
            Criterion::Only,
            "disabled=true is the inactive set"
        );
        assert_eq!(only.staff, Criterion::Only, "staff=true is the two roles");

        let except = v1_params(Some(
            "local=false&remote=false&active=false&pending=false&disabled=false&staff=false",
        ));
        assert_eq!(
            except.local,
            Criterion::Except,
            "local=false is Pleroma's external criterion"
        );
        assert_eq!(
            except.remote,
            Criterion::Except,
            "and remote=false the locals"
        );
        assert_eq!(
            except.active,
            Criterion::Except,
            "active=false is anything not fully green"
        );
        assert_eq!(
            except.pending,
            Criterion::Except,
            "pending=false is the approved set"
        );
        assert_eq!(
            except.disabled,
            Criterion::Except,
            "disabled=false is active"
        );
        assert_eq!(
            except.staff,
            Criterion::Except,
            "staff=false is nobody's role"
        );

        let mixed = v1_params(Some("active=yes&staff&pending=TRUE"));
        assert_eq!(
            mixed.pending,
            Criterion::Only,
            "case does not matter to a known spelling"
        );
        assert_eq!(
            mixed.active,
            Criterion::Any,
            "`yes` is no boolean this listing weighs"
        );
        assert_eq!(mixed.staff, Criterion::Any, "and a bare key is no value");
    }

    /// v2's words select their own criterion and leave every other one alone,
    /// so `status=pending` is `pending=true` rather than a state machine
    /// shared with `active`, and the two statuses with no column behind them
    /// state nothing at all.
    #[test]
    fn v2_reads_its_words_one_at_a_time() {
        let origin = v2_params(Some("origin=local"));
        assert_eq!(origin.local, Criterion::Only, "origin=local is the locals");
        assert_eq!(origin.remote, Criterion::Any, "and nothing else");

        let remote = v2_params(Some("origin=remote"));
        assert_eq!(
            remote.remote,
            Criterion::Only,
            "origin=remote is the mirrors"
        );
        assert_eq!(remote.local, Criterion::Any, "which is not `local` unset");

        let active = v2_params(Some("status=active"));
        assert_eq!(active.active, Criterion::Only, "status=active");
        let pending = v2_params(Some("status=pending"));
        assert_eq!(pending.pending, Criterion::Only, "status=pending");
        let disabled = v2_params(Some("status=disabled"));
        assert_eq!(disabled.disabled, Criterion::Only, "status=disabled");
        for status in ["active", "pending", "disabled"] {
            let params = v2_params(Some(&format!("status={status}")));
            let stated = [params.active, params.pending, params.disabled]
                .into_iter()
                .filter(|criterion| *criterion == Criterion::Only)
                .count();
            assert_eq!(stated, 1, "`status={status}` states exactly one criterion");
        }

        let unsupported = v2_params(Some("status=silenced&suspended=1"));
        assert_eq!(
            unsupported.active,
            Criterion::Any,
            "a silenced account is no criterion here"
        );
        assert_eq!(
            unsupported.disabled,
            Criterion::Any,
            "and neither is a suspended one"
        );
        assert_eq!(
            v2_params(Some("status=Active")).active,
            Criterion::Any,
            "the words are case-sensitive, Pleroma's clause for clause"
        );
        assert_eq!(
            v2_params(Some("permissions=staff")).staff,
            Criterion::Only,
            "permissions=staff is the staff criterion"
        );
        assert_eq!(
            v2_params(Some("permissions=admin")).staff,
            Criterion::Any,
            "any other permission level filters nothing"
        );
    }

    /// The four text criteria are the same parameters in both versions, and an
    /// empty one is nothing to filter on.
    #[test]
    fn both_versions_read_the_same_text_criteria() {
        let stated = "username=Ali&display_name=Alice&by_domain=remote.example&email=a@b.c";
        for params in [v1_params(Some(stated)), v2_params(Some(stated))] {
            assert_eq!(params.username.as_deref(), Some("Ali"), "the handle");
            assert_eq!(params.display_name.as_deref(), Some("Alice"), "the name");
            assert_eq!(
                params.domain.as_deref(),
                Some("remote.example"),
                "the mirrored host"
            );
            assert_eq!(params.email.as_deref(), Some("a@b.c"), "the address");
        }
        let empty = v1_params(Some("username=&email=&by_domain=&display_name="));
        assert!(
            empty.username.is_none()
                && empty.email.is_none()
                && empty.domain.is_none()
                && empty.display_name.is_none(),
            "an empty parameter filters nothing, in either spelling"
        );
    }

    /// A repeated criterion keeps its last value, the way a query-parameter
    /// map folds duplicates.
    #[test]
    fn a_repeated_criterion_keeps_its_last_value() {
        let params = v1_params(Some("staff=true&staff=false"));
        assert_eq!(params.staff, Criterion::Except, "the last occurrence wins");
    }

    /// The page window is Mastodon's admin pair rather than Pleroma's global
    /// one: a hundred rows when the request states no size, two hundred at
    /// most when it asks for more, and an unusable `limit` back to the
    /// default.
    #[test]
    fn the_page_window_defaults_to_a_hundred_and_caps_at_two() {
        let absent = PageParams::from_query_with_default(None, DEFAULT_LIMIT, MAX_LIMIT)
            .expect("an absent query is a first page");
        assert_eq!(absent.limit, DEFAULT_LIMIT, "the default page");
        for (query, want) in [("limit=7", 7), ("limit=150", 150), ("limit=999", MAX_LIMIT)] {
            let params = PageParams::from_query_with_default(Some(query), DEFAULT_LIMIT, MAX_LIMIT)
                .expect("a limit is parsable");
            assert_eq!(params.limit, want, "`{query}` sizes the page");
        }
        let junk =
            PageParams::from_query_with_default(Some("limit=nonsense"), DEFAULT_LIMIT, MAX_LIMIT)
                .expect("an unusable limit is no limit");
        assert_eq!(junk.limit, DEFAULT_LIMIT, "and falls back to the default");
    }

    /// The cursor half of the request still refuses a malformed id, the way
    /// every other keyset listing here does: an ignored *filter* is lenient,
    /// an unreadable *cursor* is a bad request.
    #[test]
    fn a_malformed_cursor_is_still_a_bad_request() {
        let err =
            PageParams::from_query_with_default(Some("max_id=nope"), DEFAULT_LIMIT, MAX_LIMIT)
                .expect_err("a broken cursor cannot page");
        assert_eq!(err.to_string(), "Invalid id", "the shared wording");
    }

    /// The umbrella's five words each read as their own action, spell back
    /// identically (that spelling is the moderation log's action name), and
    /// report whether they move `is_active` — the flag write and the
    /// `deactivate` entry beside it stay in step on that one answer.
    #[test]
    fn the_five_types_are_read_and_spelled_back() {
        for (word, action, moves_the_flag) in [
            ("none", AccountAction::NoAction, false),
            ("sensitive", AccountAction::Sensitive, false),
            ("silence", AccountAction::Silence, false),
            ("disable", AccountAction::Disable, true),
            ("suspend", AccountAction::Suspend, true),
        ] {
            let parsed = AccountAction::parse(Some(word))
                .unwrap_or_else(|_| panic!("`{word}` is a known type"));
            assert_eq!(parsed, action, "`{word}` is its own action");
            assert_eq!(parsed.as_str(), word, "and the entry names it back");
            assert_eq!(
                parsed.deactivates(),
                moves_the_flag,
                "`{word}`'s answer on the activation flag"
            );
        }
    }

    /// A word off the enum is the pinned 422 — Pleroma's `CastAndValidate`
    /// shape for a value outside the operation's enum — and no word at all
    /// is the other one, its `required: [:type]`. Neither reading is
    /// case-folded, trimmed or defaulted: `Disable` is no action.
    #[test]
    fn an_unusable_type_refuses_the_request() {
        for word in ["freeze", "", " ", "None", "DISABLE", " delete", "suspend "] {
            let err = AccountAction::parse(Some(word))
                .expect_err("no such word names an action to perform");
            assert_eq!(err.to_string(), INVALID_TYPE, "`{word}` is no type");
        }
        let absent = AccountAction::parse(None).expect_err("the key is required");
        assert_eq!(absent.to_string(), MISSING_TYPE, "absence refuses too");
    }

    /// The action body reads the two keys this endpoint owns — Mastodon's
    /// `type`, under its own name on the wire, and `report_id` — and lets
    /// the three it ignores go unclaimed, while a JSON `null` reads as
    /// absence for both. A `type` that is no text is the extractor's 422.
    #[test]
    fn the_action_body_reads_two_keys_and_lets_three_go() {
        let full: ActionParams = serde_json::from_value(json!({
            "type": "suspend",
            "report_id": "01934567-89ab-7cde-f012-3456789abcde",
            "text": "read the rules",
            "warning_preset_id": "7",
            "send_email_notification": true,
        }))
        .expect("the full Mastodon body");
        assert_eq!(
            full.kind.as_deref(),
            Some("suspend"),
            "the key is spelled `type` on the wire"
        );
        assert_eq!(
            full.report_id.as_deref(),
            Some("01934567-89ab-7cde-f012-3456789abcde"),
            "and the report it names arrives"
        );
        let bare: ActionParams =
            serde_json::from_value(json!({ "type": "none" })).expect("the body of the no-op");
        assert_eq!(bare.kind.as_deref(), Some("none"), "only the type");
        assert!(bare.report_id.is_none(), "and no report to resolve");
        for stated in [json!({}), json!({"type": null, "report_id": null})] {
            let none: ActionParams =
                serde_json::from_value(stated.clone()).expect("a null is absence here too");
            assert!(
                none.kind.is_none() && none.report_id.is_none(),
                "`{stated}` states nothing"
            );
        }
        assert!(
            serde_json::from_value::<ActionParams>(json!({ "type": 7 })).is_err(),
            "a type that is no text never reaches the parser"
        );
    }
}
