//! The Mastodon admin API's account endpoints over HTTP: three reads
//! (WBE-210) and five moderation writes (WBE-213).
//!
//! Every route here is core: it manages local accounts and needs no
//! `ActivityPub` data, so [`crate::api::v1::core_router`] and
//! [`crate::api::v2::core_router`] mount it whatever `PROTOCOLS` enables. The
//! listings can include remote mirror accounts and `action` can close a
//! report through `report_id`, so these routes are mixed in themselves; they
//! stay core because they read and write the accounts and reports tables
//! directly, with no `ActivityPub` machinery, and so still answer when
//! `ActivityPub` is off. The report queue is the `ActivityPub`-backed sibling,
//! [`crate::api::mastodon_admin_reports`].
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
//! The controller's `OAuthScopesPlug` pair is `admin:read:accounts` on the
//! reads and `admin:write:accounts` on the writes. All eight routes take
//! [`AdminUser`], the admin role alone. Pleroma routes the three reads
//! through `:require_privileged_role_users_read`, a privilege only
//! `admin_privileges` holds, and the entity they answer carries every
//! account's `email` and `ip`s — the same personal data the Pleroma console's
//! admin-only `/users` read serves, so the two URLs share one gate.
//! Moderation is the sharpest edge in this API and the console that owns it
//! is an admin's. A moderator's otherwise valid token is therefore the 403
//! `"User is not an admin."` on all eight. Each handler's first statement is
//! the scope half of the same pipeline: [`AdminUser::require`] with the
//! `admin:read` class on the reads or `admin:write` on the writes, both on
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
//!   `status=disabled` want [`is_active`](User::is_active) off without a
//!   suspension; `suspended` / `status=suspended` want
//!   [`User::is_suspended`] (WBE-878), so a suspended account is found by the
//!   one and not the other.
//! - `staff` (v1) and `permissions=staff` (v2) want an admin or a moderator.
//! - `username`, `display_name`, `by_domain` and `email` are the four criteria
//!   both versions state identically: the handle and the display name as
//!   case-insensitive substrings, the mirrored host and the address exactly.
//! - `silenced`, `sensitized`, `ip` and `invited_by` are accepted
//!   and ignored. This schema has no silence, sensitive-flag,
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
//!   200 and logs. `POST …/accounts/{id}/unsuspend` is its twin for the other
//!   kind of deactivation (WBE-878): `enable` reverses a `disable` and
//!   `unsuspend` a `suspend`, and each leaves an account in the other state
//!   as it is, answering 200, writing and logging nothing — Mastodon's
//!   `enable` clears `disabled` and `unsuspend` only a suspension.
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
//!   `disable` clears `is_active` and nothing else, so the account reads
//!   `disabled: true`; `suspend` clears it too and also stamps
//!   [`suspended_at`](User::suspended_at) (WBE-878), so the account reads
//!   `suspended: true` and not `disabled` — it deactivates only, stamps no
//!   `deleted_at`, revokes no token and removes no data. The `Suspended` event
//!   it fires makes `ActivityPub` queue an actor `Update` saying
//!   `"suspended": true` (no `Delete`, no `Reject` of the follows; a
//!   `disable` federates the full profile), and `…/unsuspend` reverses it
//!   completely where `…/enable` reverses a `disable`;
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
//! [`AdminUser`]: crate::auth::admin::AdminUser
//! [`AdminUser::require`]: crate::auth::admin::AdminUser::require
//! [`AdminAccount`]: crate::entities::mastodon_admin::AdminAccount
//! [`moderation_log`]: crate::models::moderation_log
//! [`helpers::start_deletion`]: crate::api::helpers::start_deletion
//! [`helpers::approve_account`]: crate::api::helpers::approve_account
//! [`admin::set_active`]: crate::models::user::admin::set_active
//! [`admin::delete_user`]: crate::models::user::admin::delete_user
//! [`User`]: crate::models::user::User

use axum::{
    Router,
    routing::{get, post},
};

use crate::state::AppState;
#[cfg(doc)]
use crate::{
    api::helpers::link_query,
    models::user::{User, admin},
    pagination::PageParams,
};

mod actions;
mod reads;
mod writes;

use self::{
    actions::account_action,
    reads::{list_accounts, list_accounts_v2, show_account},
    writes::{approve_account, delete_account, enable_account, reject_account, unsuspend_account},
};

/// The read endpoint class of [`AdminUser::require`].
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
/// The `status` value that weighs a suspended account.
const STATUS_SUSPENDED: &str = "suspended";
/// The `permissions` value that weighs the two staff flags.
const PERMISSIONS_STAFF: &str = "staff";
/// The 403 body of the two refusals this surface answers alike: the admin
/// who names their own account in a deletion, and the rejection of an
/// account nobody had left pending. Pleroma reaches neither — its `delete`
/// runs no self check and its `User.reject` answers an approved account with
/// `{:error, "User is approved"}` and a 400 — the ticket pins this one
/// wording for both.
const NOT_ALLOWED: &str = "This action is not allowed";
/// The 400 body of a `disable` or `suspend` aimed at a remote mirror: this
/// instance owns no flag of a remote account's, and Pleroma's
/// `validate_account_action` refuses the same way.
const ONLY_LOCAL: &str = "Only local accounts can be disabled";
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
        .route("/accounts/{id}/unsuspend", post(unsuspend_account))
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
