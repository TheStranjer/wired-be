//! The `users` table: one table for local accounts and remote actors.
//!
//! Per `docs/ARCHITECTURE.md` decision 7 a local user (`alice`) and a
//! remote mirror (`alice@remote.example`) share the table, told apart by
//! the `local` flag. [`NewUser::default`] mirrors the column defaults, and
//! [`NewUser::local`] is the usual starting point for a local account. The
//! reference shape is Pleroma's `User` schema (`lib/pleroma/user.ex`); the
//! ticket columns are the authoritative subset.
//!
//! Denormalised counters move only through [`update_counts`], which is
//! atomic in SQL and clamps every count at zero. The URL of a local actor
//! and its endpoints come from [`local_ap_id`] and the helpers beside it,
//! so federation and the API build them the same way.
//!
//! `password_hash`, `private_key` and `mfa_settings` (which holds the TOTP
//! secret) are secrets: the manual `Debug` impls in this module redact
//! them, so they can never reach a log line.
//!
//! Registration and password login live in [`register`]; the MFA column's
//! shape and updates live in [`mfa`]; the email-confirmation token, its
//! letter and its two writers live in [`confirmation`].

use std::fmt;

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{Value, json};
use sqlx::{Executor, PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::{config::Config, error::AppResult, pagination::PageParams, util::id::new_id};

pub mod confirmation;
pub mod mfa;
pub mod notification_settings;
pub mod register;

/// `actor_type` a [`NewUser`] starts with: the `ActivityPub` default actor
/// kind, matching the column default.
pub const DEFAULT_ACTOR_TYPE: &str = "Person";

/// `default_scope` a [`NewUser`] starts with: the visibility new statuses
/// get, matching the column default (Pleroma's `:defaults` `access`).
pub const DEFAULT_SCOPE: &str = "public";

/// The nickname of the instance actor: the local `Application` account
/// that signs this instance's fetches and owns server-level activities
/// (see [`crate::services::instance_actor`]).
///
/// It is deliberately not a registrable handle and never a listed account.
pub const INSTANCE_ACTOR_NICKNAME: &str = "internal.fetch";

/// Shared SQL fragment appending the exclusion of the instance actor to a
/// `WHERE` clause: ` AND nickname <> 'internal.fetch'`.
///
/// Every query that counts or lists users for a public listing (instance
/// stats, `NodeInfo` counters, and their search/directory/suggestions
/// counterparts when they arrive) ends its predicate with this fragment, so
/// the actor never shows up in them; lookups that name the account directly
/// ([`get_by_nickname`], [`get_by_acct`]) are allowed to find it.
///
/// The literal is kept in step with [`INSTANCE_ACTOR_NICKNAME`] by a test.
pub const NOT_INSTANCE_ACTOR: &str = " AND nickname <> 'internal.fetch'";

/// A Postgres `boolean` column as a named flag.
///
/// `User` and `NewUser` carry twenty booleans each; Clippy's
/// `struct_excessive_bools` (denied, threshold 2) wants them modelled, not
/// piled up, so every boolean column is a `Flag`. The `sqlx` transparent
/// derive makes it encode, decode and compare exactly like the inner
/// `bool`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct Flag(bool);

impl Flag {
    /// Wraps a raw boolean as a flag.
    #[must_use]
    pub const fn new(value: bool) -> Self {
        Self(value)
    }

    /// The raw boolean behind the flag.
    #[must_use]
    pub const fn get(self) -> bool {
        self.0
    }
}

/// A `bool` is already a valid flag value; `true.into()` sets one.
impl From<bool> for Flag {
    fn from(value: bool) -> Self {
        Self(value)
    }
}

/// Where an account stands between registration and use: Pleroma's
/// `User.account_status` (`lib/pleroma/user.ex`) over the states this
/// server tracks.
///
/// They are weighed in Pleroma's order — a deactivated account outranks an
/// unapproved one, which outranks an unconfirmed one. (Pleroma's
/// `:password_reset_pending` has no column here; the password-reset ticket
/// adds its arm.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountStatus {
    /// Nothing waits: the account logs in and uses its tokens.
    Active,
    /// [`is_active`](User::is_active) is false.
    Deactivated,
    /// Staff have not approved the registration
    /// ([`approval_pending`](User::approval_pending) is set).
    ApprovalPending,
    /// The email address is not confirmed
    /// ([`email_confirmed`](User::email_confirmed) is false).
    ConfirmationPending,
}

/// One row of `users`: a local account or a mirror of a remote actor
/// (decision 7).
#[derive(sqlx::FromRow)]
pub struct User {
    /// `UUIDv7` primary key, minted by [`insert`] (decision 4).
    pub id: Uuid,
    /// Handle: `alice` locally, `alice@remote.example` for remote users.
    /// Case-insensitive (citext), unique.
    pub nickname: String,
    /// `ActivityPub` actor id, unique.
    pub ap_id: String,
    /// Whether this instance owns the account.
    pub local: Flag,
    /// Host of a remote actor; `None` for local users.
    pub domain: Option<String>,
    /// `ActivityPub` actor type (`Person`, `Service`, ...).
    pub actor_type: String,
    /// Account email, local users only; case-insensitive, unique.
    pub email: Option<String>,
    /// PHC password hash, local users only. Never in `Debug` output.
    pub password_hash: Option<String>,
    /// Display name; empty means "same as the nickname".
    pub name: String,
    /// Profile note as sanitized HTML.
    pub bio: String,
    /// Profile note as its source text.
    pub raw_bio: String,
    /// Avatar URL, if set.
    pub avatar_url: Option<String>,
    /// Banner URL, if set.
    pub banner_url: Option<String>,
    /// Human-facing profile URL (the `ActivityPub` `url`), if set.
    pub profile_url: Option<String>,
    /// Whether follow requests need manual approval.
    pub locked: Flag,
    /// Whether the account is flagged as automated.
    pub bot: Flag,
    /// Whether the account opted into discovery.
    pub discoverable: Flag,
    /// Profile metadata: JSON array of `{name, value}` objects whose value
    /// is the rendered HTML (see [`raw_fields`](Self::raw_fields) for the
    /// editing text behind it).
    pub fields: Value,
    /// Profile metadata as its source text: JSON array of `{name, value}`
    /// objects, the same entries [`fields`](Self::fields) renders.
    pub raw_fields: Value,
    /// `ActivityPub` inbox endpoint, if known.
    pub inbox: Option<String>,
    /// Shared inbox endpoint, if known.
    pub shared_inbox: Option<String>,
    /// `ActivityPub` outbox collection id, if known.
    pub outbox: Option<String>,
    /// Followers collection id, if known.
    pub followers_url: Option<String>,
    /// Following collection id, if known.
    pub following_url: Option<String>,
    /// Featured (pinned) collection id, if known.
    pub featured_url: Option<String>,
    /// Actor public signing key (PEM), if known.
    pub public_key: Option<String>,
    /// Actor private signing key; local users only. Never in `Debug`
    /// output.
    pub private_key: Option<String>,
    /// Whether the account has the admin role.
    pub is_admin: Flag,
    /// Whether the account has the moderator role.
    pub is_moderator: Flag,
    /// Whether the account is active (not deactivated or suspended).
    pub is_active: Flag,
    /// Whether the follower collection is hidden from other accounts.
    pub hide_followers: Flag,
    /// Whether the follow collection is hidden from other accounts.
    pub hide_follows: Flag,
    /// Whether [`followers_count`](Self::followers_count) renders as zero
    /// for everyone but the owner.
    pub hide_followers_count: Flag,
    /// Whether [`following_count`](Self::following_count) renders as zero
    /// for everyone but the owner.
    pub hide_follows_count: Flag,
    /// Whether the favourites list is private to the account.
    pub hide_favorites: Flag,
    /// Whether a `Move` of this account may take the following list with
    /// it (Pleroma's `allow_following_move`).
    pub allow_following_move: Flag,
    /// Whether this account's statuses skip thread containment.
    pub skip_thread_containment: Flag,
    /// Whether the account accepts chat messages. A remote mirror keeps
    /// the column default until federation sees the capability; see
    /// [`Account`](crate::entities::account::Account) for how that
    /// renders.
    pub accepts_chat_messages: Flag,
    /// Whether the admin/moderator flags may be shown to other accounts.
    pub show_role: Flag,
    /// Whether the account asked clients not to render rich text.
    pub no_rich_text: Flag,
    /// Visibility new statuses default to (`public`, `unlisted`,
    /// `private`, `direct`).
    pub default_scope: String,
    /// Whether new statuses are flagged sensitive by default; the
    /// `source.sensitive` of the entity, `false` until a profile update
    /// says otherwise.
    pub sensitive_default: Flag,
    /// BCP 47 language tag new statuses default to, the `source.language`
    /// of the entity; `None` when the account stated no preference.
    pub language: Option<String>,
    /// Profile tags (`pleroma.tags`).
    pub tags: Vec<String>,
    /// Client settings the server only stores and round-trips
    /// (`pleroma.settings_store`): a JSON object.
    pub settings_store: Value,
    /// Per-account notification flags: the jsonb blob parsed by
    /// [`notification_settings::NotificationSettings`]
    /// (`block_from_strangers`, `hide_notification_contents`), written by
    /// `PUT /api/pleroma/notification_settings` and read by
    /// [`maybe_notify`](crate::models::notification::maybe_notify) and the
    /// account entity.
    pub notification_settings: Value,
    /// Profile background image URL, if set (unlike the avatar and banner
    /// it has no instance default).
    pub background_image_url: Option<String>,
    /// The account's birthday, if it gave one.
    pub birthday: Option<NaiveDate>,
    /// Whether strangers may see [`birthday`](Self::birthday).
    pub show_birthday: Flag,
    /// Other actor ids this account is also known as.
    pub also_known_as: Vec<String>,
    /// Actor id this account moved to, if it moved; the entity's top-level
    /// `moved` renders from it.
    pub moved_to_ap_id: Option<String>,
    /// When the account last moved; `None` when it never did. The move
    /// endpoint's once-per-30-days cooldown reads it.
    pub last_move_at: Option<DateTime<Utc>>,
    /// Whether the account email has been confirmed.
    pub email_confirmed: Flag,
    /// Whether a moderator still has to approve the account.
    pub approval_pending: Flag,
    /// The secret token the confirmation link carries, `None` once the
    /// address is confirmed (or for an account that never had to confirm
    /// one). Secret material: redacted from `Debug` output.
    pub confirmation_token: Option<String>,
    /// When the confirmation email bearing [`confirmation_token`](Self::confirmation_token)
    /// was queued; `None` when none ever was.
    pub confirmation_sent_at: Option<DateTime<Utc>>,
    /// The `reason` text given at registration, kept for the later
    /// approval flow (Pleroma's `users.registration_reason`); `None` when
    /// the account stated none.
    pub registration_reason: Option<String>,
    /// When the account was last seen; `None` when it never was.
    pub last_active_at: Option<DateTime<Utc>>,
    /// Number of followers; moved by [`update_counts`].
    pub followers_count: i32,
    /// Number of follows; moved by [`update_counts`].
    pub following_count: i32,
    /// Number of posted statuses; moved by [`update_counts`].
    pub statuses_count: i32,
    /// Insert time of the newest status this user posted.
    pub last_status_at: Option<DateTime<Utc>>,
    /// When a remote actor was last fetched.
    pub last_fetched_at: Option<DateTime<Utc>>,
    /// When the row was inserted.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by every update here.
    pub updated_at: DateTime<Utc>,
    /// Multi-factor authentication state: the jsonb blob parsed by
    /// [`mfa::MfaSettings`] (enabled flag, TOTP secret and confirmation,
    /// backup-code hashes). Holds the TOTP secret; never in `Debug`
    /// output.
    pub mfa_settings: Value,
    /// The frontend the account picked at `PUT /api/v1/pleroma/preferred_frontend`;
    /// `None` when it never chose one. The `preferred_frontend` cookie
    /// mirrors it (Pleroma keeps the choice in the cookie alone).
    pub preferred_frontend: Option<String>,
    /// Whether an admin put the account in the suggestion pool the
    /// `GET /api/v{1,2}/suggestions` endpoints answer from
    /// (Pleroma's `is_suggested`). Nothing here sets it: the admin API
    /// (a later ticket) is the writer.
    pub is_suggested: Flag,
    /// Whether the account must change its password before it may log in
    /// again (Pleroma's `password_reset_pending`): the admin force-reset
    /// flow (a later ticket) sets it, and completing a password reset
    /// clears it again. `check_account_ready` refuses a pending account.
    pub password_reset_pending: Flag,
    /// Whether the account wants email from the instance (Pleroma's
    /// `email_notifications`, moved by `User.switch_email_notifications`).
    /// The HTML unsubscribe page (a later ticket) clears it; nothing reads
    /// it yet.
    pub email_notifications: Flag,
    /// When the account started its deletion (`POST
    /// /api/pleroma/delete_account`, see [`mark_deleted`]); `None` while
    /// it lives. The row is kept for referential integrity even after the
    /// purge; this is what tells a deleted account from a merely
    /// deactivated one.
    pub deleted_at: Option<DateTime<Utc>>,
}

/// [`password_hash`](Self::password_hash), [`private_key`](Self::private_key)
/// and [`mfa_settings`](Self::mfa_settings) are secrets and render as
/// `<redacted>`; everything else shows.
impl fmt::Debug for User {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("User")
            .field("id", &self.id)
            .field("nickname", &self.nickname)
            .field("ap_id", &self.ap_id)
            .field("local", &self.local)
            .field("domain", &self.domain)
            .field("actor_type", &self.actor_type)
            .field("email", &self.email)
            .field("password_hash", &"<redacted>")
            .field("name", &self.name)
            .field("bio", &self.bio)
            .field("raw_bio", &self.raw_bio)
            .field("avatar_url", &self.avatar_url)
            .field("banner_url", &self.banner_url)
            .field("profile_url", &self.profile_url)
            .field("locked", &self.locked)
            .field("bot", &self.bot)
            .field("discoverable", &self.discoverable)
            .field("fields", &self.fields)
            .field("raw_fields", &self.raw_fields)
            .field("inbox", &self.inbox)
            .field("shared_inbox", &self.shared_inbox)
            .field("outbox", &self.outbox)
            .field("followers_url", &self.followers_url)
            .field("following_url", &self.following_url)
            .field("featured_url", &self.featured_url)
            .field("public_key", &self.public_key)
            .field("private_key", &"<redacted>")
            .field("is_admin", &self.is_admin)
            .field("is_moderator", &self.is_moderator)
            .field("is_active", &self.is_active)
            .field("hide_followers", &self.hide_followers)
            .field("hide_follows", &self.hide_follows)
            .field("hide_followers_count", &self.hide_followers_count)
            .field("hide_follows_count", &self.hide_follows_count)
            .field("hide_favorites", &self.hide_favorites)
            .field("allow_following_move", &self.allow_following_move)
            .field("skip_thread_containment", &self.skip_thread_containment)
            .field("accepts_chat_messages", &self.accepts_chat_messages)
            .field("show_role", &self.show_role)
            .field("no_rich_text", &self.no_rich_text)
            .field("default_scope", &self.default_scope)
            .field("sensitive_default", &self.sensitive_default)
            .field("language", &self.language)
            .field("tags", &self.tags)
            .field("settings_store", &self.settings_store)
            .field("notification_settings", &self.notification_settings)
            .field("background_image_url", &self.background_image_url)
            .field("birthday", &self.birthday)
            .field("show_birthday", &self.show_birthday)
            .field("also_known_as", &self.also_known_as)
            .field("moved_to_ap_id", &self.moved_to_ap_id)
            .field("last_move_at", &self.last_move_at)
            .field("email_confirmed", &self.email_confirmed)
            .field("approval_pending", &self.approval_pending)
            .field("confirmation_token", &"<redacted>")
            .field("confirmation_sent_at", &self.confirmation_sent_at)
            .field("registration_reason", &self.registration_reason)
            .field("last_active_at", &self.last_active_at)
            .field("followers_count", &self.followers_count)
            .field("following_count", &self.following_count)
            .field("statuses_count", &self.statuses_count)
            .field("last_status_at", &self.last_status_at)
            .field("last_fetched_at", &self.last_fetched_at)
            .field("inserted_at", &self.inserted_at)
            .field("updated_at", &self.updated_at)
            .field("mfa_settings", &"<redacted>")
            .field("preferred_frontend", &self.preferred_frontend)
            .field("is_suggested", &self.is_suggested)
            .field("password_reset_pending", &self.password_reset_pending)
            .field("email_notifications", &self.email_notifications)
            .field("deleted_at", &self.deleted_at)
            .finish()
    }
}

impl User {
    /// Where this account stands between registration and use, weighed in
    /// the order Pleroma's `User.account_status` uses: deactivation, then
    /// the staff approval `REGISTRATIONS_APPROVAL_REQUIRED` asks for, then
    /// the email confirmation `ACCOUNT_ACTIVATION_REQUIRED` asks for.
    ///
    /// The two registration flags are only ever cleared for local accounts
    /// (a federated mirror arrives confirmed and approved), so no `local`
    /// arm is needed to reproduce Pleroma's. The login gate of
    /// [`crate::api::oauth`] and the bearer extractors of [`crate::auth`]
    /// both decide from this one read.
    #[must_use]
    pub const fn account_status(&self) -> AccountStatus {
        if !self.is_active.get() {
            return AccountStatus::Deactivated;
        }
        if self.approval_pending.get() {
            return AccountStatus::ApprovalPending;
        }
        if !self.email_confirmed.get() {
            return AccountStatus::ConfirmationPending;
        }
        AccountStatus::Active
    }
}

/// What to insert: every `users` column except the ones [`insert`] mints
/// itself (`id`) or the database stamps (`inserted_at`, `updated_at`).
///
/// [`NewUser::default`] gives the column defaults;
/// [`NewUser::local`] is the usual constructor for a local account.
#[derive(Clone)]
pub struct NewUser {
    /// Handle to store (case folds happen in citext).
    pub nickname: String,
    /// `ActivityPub` actor id.
    pub ap_id: String,
    /// Whether the account is owned by this instance.
    pub local: Flag,
    /// Host of the remote actor; `None` for local users.
    pub domain: Option<String>,
    /// `ActivityPub` actor type; [`DEFAULT_ACTOR_TYPE`] by default.
    pub actor_type: String,
    /// Account email, for local users.
    pub email: Option<String>,
    /// PHC password hash. Never in `Debug` output.
    pub password_hash: Option<String>,
    /// Display name; empty by default.
    pub name: String,
    /// Profile note as sanitized HTML; empty by default.
    pub bio: String,
    /// Profile note as its source text; empty by default.
    pub raw_bio: String,
    /// Avatar URL.
    pub avatar_url: Option<String>,
    /// Banner URL.
    pub banner_url: Option<String>,
    /// Human-facing profile URL (the `ActivityPub` `url`).
    pub profile_url: Option<String>,
    /// Whether follows require approval; `false` by default.
    pub locked: Flag,
    /// Whether the account is automated; `false` by default.
    pub bot: Flag,
    /// Whether the account opted into discovery; `false` by default.
    pub discoverable: Flag,
    /// Profile metadata; the empty JSON array by default.
    pub fields: Value,
    /// Profile metadata as its source text; the empty JSON array by
    /// default.
    pub raw_fields: Value,
    /// `ActivityPub` inbox endpoint.
    pub inbox: Option<String>,
    /// Shared inbox endpoint.
    pub shared_inbox: Option<String>,
    /// `ActivityPub` outbox collection id.
    pub outbox: Option<String>,
    /// Followers collection id.
    pub followers_url: Option<String>,
    /// Following collection id.
    pub following_url: Option<String>,
    /// Featured (pinned) collection id.
    pub featured_url: Option<String>,
    /// Actor public signing key (PEM).
    pub public_key: Option<String>,
    /// Actor private signing key, local users only. Never in `Debug`
    /// output.
    pub private_key: Option<String>,
    /// Whether the account is an admin; `false` by default.
    pub is_admin: Flag,
    /// Whether the account is a moderator; `false` by default.
    pub is_moderator: Flag,
    /// Whether the account is active; `true` by default.
    pub is_active: Flag,
    /// Whether the follower collection is hidden; `false` by default.
    pub hide_followers: Flag,
    /// Whether the follow collection is hidden; `false` by default.
    pub hide_follows: Flag,
    /// Whether the follower count is hidden; `false` by default.
    pub hide_followers_count: Flag,
    /// Whether the follow count is hidden; `false` by default.
    pub hide_follows_count: Flag,
    /// Whether favourites are private; `true` by default.
    pub hide_favorites: Flag,
    /// Whether a move may take the following list; `true` by default.
    pub allow_following_move: Flag,
    /// Whether statuses skip thread containment; `false` by default.
    pub skip_thread_containment: Flag,
    /// Whether the account accepts chat messages; `true` by default.
    pub accepts_chat_messages: Flag,
    /// Whether the role flags may be shown; `true` by default.
    pub show_role: Flag,
    /// Whether the account asked for plain text; `false` by default.
    pub no_rich_text: Flag,
    /// Visibility new statuses default to; [`DEFAULT_SCOPE`] by default.
    pub default_scope: String,
    /// Whether new statuses are flagged sensitive by default; `false` by
    /// default, the `source.sensitive` of the entity.
    pub sensitive_default: Flag,
    /// BCP 47 language tag new statuses default to; `None` (no preference)
    /// by default, the `source.language` of the entity.
    pub language: Option<String>,
    /// Profile tags; the empty array by default.
    pub tags: Vec<String>,
    /// Client settings blob; the empty JSON object by default.
    pub settings_store: Value,
    /// Per-account notification flags; the all-off
    /// [`notification_settings::NotificationSettings`] blob by default,
    /// matching the column default.
    pub notification_settings: Value,
    /// Profile background image URL.
    pub background_image_url: Option<String>,
    /// The account's birthday, if any.
    pub birthday: Option<NaiveDate>,
    /// Whether strangers may see the birthday; `false` by default.
    pub show_birthday: Flag,
    /// Other actor ids this account is also known as; empty by default.
    pub also_known_as: Vec<String>,
    /// Actor id this account moved to, if it moved.
    pub moved_to_ap_id: Option<String>,
    /// When the account last moved; `None` (never moved) by default.
    pub last_move_at: Option<DateTime<Utc>>,
    /// Whether the email is confirmed; `true` by default.
    pub email_confirmed: Flag,
    /// Whether the account awaits moderator approval; `false` by default.
    pub approval_pending: Flag,
    /// The confirmation token to store; `None` (`no confirmation owed`) by
    /// default. Secret material: redacted from `Debug` output.
    pub confirmation_token: Option<String>,
    /// When the confirmation email was queued; `None` by default.
    pub confirmation_sent_at: Option<DateTime<Utc>>,
    /// The registration `reason` text to store; `None` (no reason stated)
    /// by default.
    pub registration_reason: Option<String>,
    /// When the account was last seen.
    pub last_active_at: Option<DateTime<Utc>>,
    /// Initial follower count; `0` by default.
    pub followers_count: i32,
    /// Initial following count; `0` by default.
    pub following_count: i32,
    /// Initial status count; `0` by default.
    pub statuses_count: i32,
    /// Initial newest-status time; `None` by default.
    pub last_status_at: Option<DateTime<Utc>>,
    /// Initial last-fetch time; `None` by default.
    pub last_fetched_at: Option<DateTime<Utc>>,
    /// Multi-factor authentication state; the all-off [`mfa::MfaSettings`]
    /// blob by default, matching the column default. Holds the TOTP
    /// secret; never in `Debug` output.
    pub mfa_settings: Value,
    /// The frontend the account picked; `None` (no choice) by default.
    pub preferred_frontend: Option<String>,
    /// Whether the account sits in the suggestion pool; `false` by default,
    /// because only an admin puts it there (the admin API is a later
    /// ticket).
    pub is_suggested: Flag,
    /// Whether the account must reset its password before logging in;
    /// `false` by default, because only the admin force-reset flow (a
    /// later ticket) puts an account in that state.
    pub password_reset_pending: Flag,
    /// Whether the account wants email from the instance; `true` by
    /// default, matching the column default (Pleroma's
    /// `email_notifications`).
    pub email_notifications: Flag,
}

/// [`password_hash`](Self::password_hash) and [`private_key`](Self::private_key)
/// are secrets and render as `<redacted>`; [`mfa_settings`](Self::mfa_settings)
/// holds the TOTP secret and is redacted too; everything else shows.
impl fmt::Debug for NewUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewUser")
            .field("nickname", &self.nickname)
            .field("ap_id", &self.ap_id)
            .field("local", &self.local)
            .field("domain", &self.domain)
            .field("actor_type", &self.actor_type)
            .field("email", &self.email)
            .field("password_hash", &"<redacted>")
            .field("name", &self.name)
            .field("bio", &self.bio)
            .field("raw_bio", &self.raw_bio)
            .field("avatar_url", &self.avatar_url)
            .field("banner_url", &self.banner_url)
            .field("profile_url", &self.profile_url)
            .field("locked", &self.locked)
            .field("bot", &self.bot)
            .field("discoverable", &self.discoverable)
            .field("fields", &self.fields)
            .field("raw_fields", &self.raw_fields)
            .field("inbox", &self.inbox)
            .field("shared_inbox", &self.shared_inbox)
            .field("outbox", &self.outbox)
            .field("followers_url", &self.followers_url)
            .field("following_url", &self.following_url)
            .field("featured_url", &self.featured_url)
            .field("public_key", &self.public_key)
            .field("private_key", &"<redacted>")
            .field("is_admin", &self.is_admin)
            .field("is_moderator", &self.is_moderator)
            .field("is_active", &self.is_active)
            .field("hide_followers", &self.hide_followers)
            .field("hide_follows", &self.hide_follows)
            .field("hide_followers_count", &self.hide_followers_count)
            .field("hide_follows_count", &self.hide_follows_count)
            .field("hide_favorites", &self.hide_favorites)
            .field("allow_following_move", &self.allow_following_move)
            .field("skip_thread_containment", &self.skip_thread_containment)
            .field("accepts_chat_messages", &self.accepts_chat_messages)
            .field("show_role", &self.show_role)
            .field("no_rich_text", &self.no_rich_text)
            .field("default_scope", &self.default_scope)
            .field("sensitive_default", &self.sensitive_default)
            .field("language", &self.language)
            .field("tags", &self.tags)
            .field("settings_store", &self.settings_store)
            .field("notification_settings", &self.notification_settings)
            .field("background_image_url", &self.background_image_url)
            .field("birthday", &self.birthday)
            .field("show_birthday", &self.show_birthday)
            .field("also_known_as", &self.also_known_as)
            .field("moved_to_ap_id", &self.moved_to_ap_id)
            .field("last_move_at", &self.last_move_at)
            .field("email_confirmed", &self.email_confirmed)
            .field("approval_pending", &self.approval_pending)
            .field("confirmation_token", &"<redacted>")
            .field("confirmation_sent_at", &self.confirmation_sent_at)
            .field("registration_reason", &self.registration_reason)
            .field("last_active_at", &self.last_active_at)
            .field("followers_count", &self.followers_count)
            .field("following_count", &self.following_count)
            .field("statuses_count", &self.statuses_count)
            .field("last_status_at", &self.last_status_at)
            .field("last_fetched_at", &self.last_fetched_at)
            .field("mfa_settings", &"<redacted>")
            .field("preferred_frontend", &self.preferred_frontend)
            .field("is_suggested", &self.is_suggested)
            .field("password_reset_pending", &self.password_reset_pending)
            .field("email_notifications", &self.email_notifications)
            .finish()
    }
}

/// The column defaults of `users`: a remote `Person` with no contact
/// details, zero counts and nothing disabled.
impl Default for NewUser {
    fn default() -> Self {
        Self {
            nickname: String::new(),
            ap_id: String::new(),
            local: Flag::new(false),
            domain: None,
            actor_type: DEFAULT_ACTOR_TYPE.to_owned(),
            email: None,
            password_hash: None,
            name: String::new(),
            bio: String::new(),
            raw_bio: String::new(),
            avatar_url: None,
            banner_url: None,
            profile_url: None,
            locked: Flag::new(false),
            bot: Flag::new(false),
            discoverable: Flag::new(false),
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
            is_admin: Flag::new(false),
            is_moderator: Flag::new(false),
            is_active: Flag::new(true),
            hide_followers: Flag::new(false),
            hide_follows: Flag::new(false),
            hide_followers_count: Flag::new(false),
            hide_follows_count: Flag::new(false),
            hide_favorites: Flag::new(true),
            allow_following_move: Flag::new(true),
            skip_thread_containment: Flag::new(false),
            accepts_chat_messages: Flag::new(true),
            show_role: Flag::new(true),
            no_rich_text: Flag::new(false),
            default_scope: DEFAULT_SCOPE.to_owned(),
            sensitive_default: Flag::new(false),
            language: None,
            tags: Vec::new(),
            settings_store: json!({}),
            notification_settings: notification_settings::NotificationSettings::default_value(),
            background_image_url: None,
            birthday: None,
            show_birthday: Flag::new(false),
            also_known_as: Vec::new(),
            moved_to_ap_id: None,
            last_move_at: None,
            email_confirmed: Flag::new(true),
            approval_pending: Flag::new(false),
            confirmation_token: None,
            confirmation_sent_at: None,
            registration_reason: None,
            last_active_at: None,
            followers_count: 0,
            following_count: 0,
            statuses_count: 0,
            last_status_at: None,
            last_fetched_at: None,
            mfa_settings: mfa::MfaSettings::default_value(),
            preferred_frontend: None,
            is_suggested: Flag::new(false),
            password_reset_pending: Flag::new(false),
            email_notifications: Flag::new(true),
        }
    }
}

impl NewUser {
    /// A new local account named `nickname` with actor id `ap_id`: the
    /// column defaults of [`NewUser::default`] plus `local = true`. Pair
    /// it with [`local_ap_id`] to build `ap_id`.
    #[must_use]
    pub fn local(nickname: &str, ap_id: &str) -> Self {
        Self {
            nickname: nickname.to_owned(),
            ap_id: ap_id.to_owned(),
            local: Flag::new(true),
            ..Self::default()
        }
    }
}

/// One step to apply to the denormalised counters of a user atomically.
///
/// Each field is added to its column by [`update_counts`]; negatives
/// shrink the count, and the result is clamped at zero.
#[derive(Debug, Default, Clone, Copy)]
pub struct CountDelta {
    /// Step for `followers_count`.
    pub followers: i32,
    /// Step for `following_count`.
    pub following: i32,
    /// Step for `statuses_count`.
    pub statuses: i32,
}

/// The `INSERT` behind [`insert`]: every column is written from a
/// [`NewUser`] (the id is the extra bind), with `RETURNING *` so the row
/// comes back with its database-side timestamps.
const INSERT_SQL: &str = "INSERT INTO users (
     id, nickname, ap_id, local, domain, actor_type, email, password_hash,
     name, bio, raw_bio, avatar_url, banner_url, profile_url, locked, bot,
     discoverable, fields, inbox, shared_inbox, outbox, followers_url,
     following_url, featured_url, public_key, private_key, is_admin,
     is_moderator, is_active, hide_followers, hide_follows,
     hide_followers_count, hide_follows_count, hide_favorites,
     allow_following_move, skip_thread_containment, accepts_chat_messages,
     show_role, no_rich_text, default_scope, sensitive_default, language,
     tags, settings_store, background_image_url, birthday, show_birthday,
     also_known_as, moved_to_ap_id, last_move_at, email_confirmed,
     approval_pending, last_active_at, followers_count, following_count,
     statuses_count, last_status_at, last_fetched_at, mfa_settings,
     preferred_frontend, raw_fields, is_suggested, registration_reason,
     password_reset_pending, notification_settings, email_notifications,
     confirmation_token, confirmation_sent_at
) VALUES (
     $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
     $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28,
     $29, $30, $31, $32, $33, $34, $35, $36, $37, $38, $39, $40, $41,
     $42, $43, $44, $45, $46, $47, $48, $49, $50, $51, $52, $53, $54,
     $55, $56, $57, $58, $59, $60, $61, $62, $63, $64, $65, $66, $67, $68
) RETURNING *";

/// Inserts `new_user` on `executor` — the pool or an open transaction —
/// and returns the stored row.
///
/// The id is a fresh `UUIDv7` (decision 4); every other column comes from
/// `new_user`, whose [`Default`](NewUser::default) mirrors the column
/// defaults. `nickname`, `ap_id` and `email` are unique, and citext folds
/// case in the nickname and email comparisons, so a duplicate in any case
/// is rejected.
///
/// # Errors
///
/// Any database failure (connection, constraint, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError); a unique violation
/// (SQLSTATE `23505`) is reported through it unchanged.
pub async fn insert<'e, E>(executor: E, new_user: &NewUser) -> AppResult<User>
where
    E: Executor<'e, Database = Postgres>,
{
    let user = sqlx::query_as::<_, User>(INSERT_SQL)
        .bind(new_id())
        .bind(new_user.nickname.as_str())
        .bind(new_user.ap_id.as_str())
        .bind(new_user.local)
        .bind(new_user.domain.as_deref())
        .bind(new_user.actor_type.as_str())
        .bind(new_user.email.as_deref())
        .bind(new_user.password_hash.as_deref())
        .bind(new_user.name.as_str())
        .bind(new_user.bio.as_str())
        .bind(new_user.raw_bio.as_str())
        .bind(new_user.avatar_url.as_deref())
        .bind(new_user.banner_url.as_deref())
        .bind(new_user.profile_url.as_deref())
        .bind(new_user.locked)
        .bind(new_user.bot)
        .bind(new_user.discoverable)
        .bind(new_user.fields.clone())
        .bind(new_user.inbox.as_deref())
        .bind(new_user.shared_inbox.as_deref())
        .bind(new_user.outbox.as_deref())
        .bind(new_user.followers_url.as_deref())
        .bind(new_user.following_url.as_deref())
        .bind(new_user.featured_url.as_deref())
        .bind(new_user.public_key.as_deref())
        .bind(new_user.private_key.as_deref())
        .bind(new_user.is_admin)
        .bind(new_user.is_moderator)
        .bind(new_user.is_active)
        .bind(new_user.hide_followers)
        .bind(new_user.hide_follows)
        .bind(new_user.hide_followers_count)
        .bind(new_user.hide_follows_count)
        .bind(new_user.hide_favorites)
        .bind(new_user.allow_following_move)
        .bind(new_user.skip_thread_containment)
        .bind(new_user.accepts_chat_messages)
        .bind(new_user.show_role)
        .bind(new_user.no_rich_text)
        .bind(new_user.default_scope.as_str())
        .bind(new_user.sensitive_default)
        .bind(new_user.language.as_deref())
        .bind(&new_user.tags)
        .bind(new_user.settings_store.clone())
        .bind(new_user.background_image_url.as_deref())
        .bind(new_user.birthday)
        .bind(new_user.show_birthday)
        .bind(&new_user.also_known_as)
        .bind(new_user.moved_to_ap_id.as_deref())
        .bind(new_user.last_move_at)
        .bind(new_user.email_confirmed)
        .bind(new_user.approval_pending)
        .bind(new_user.last_active_at)
        .bind(new_user.followers_count)
        .bind(new_user.following_count)
        .bind(new_user.statuses_count)
        .bind(new_user.last_status_at)
        .bind(new_user.last_fetched_at)
        .bind(new_user.mfa_settings.clone())
        .bind(new_user.preferred_frontend.as_deref())
        .bind(new_user.raw_fields.clone())
        .bind(new_user.is_suggested)
        .bind(new_user.registration_reason.as_deref())
        .bind(new_user.password_reset_pending)
        .bind(new_user.notification_settings.clone())
        .bind(new_user.email_notifications)
        .bind(new_user.confirmation_token.as_deref())
        .bind(new_user.confirmation_sent_at)
        .fetch_one(executor)
        .await?;
    Ok(user)
}

/// Fetches the user whose primary key is `id`, `None` when no row matches.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_id(pool: &PgPool, id: Uuid) -> AppResult<Option<User>> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(user)
}

/// Fetches the user with nickname `nickname`, `None` when absent.
///
/// The comparison is case-insensitive because the column is citext, so
/// `ALICE` finds `alice`; the bind is cast to citext because a bare text
/// parameter would resolve to Postgres' case-sensitive `text = text`
/// operator.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_nickname(pool: &PgPool, nickname: &str) -> AppResult<Option<User>> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE nickname = $1::citext")
        .bind(nickname)
        .fetch_optional(pool)
        .await?;
    Ok(user)
}

/// Fetches the user with `ActivityPub` actor id `ap_id`, `None` when absent.
/// The comparison is exact (plain `text`).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_ap_id(pool: &PgPool, ap_id: &str) -> AppResult<Option<User>> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE ap_id = $1")
        .bind(ap_id)
        .fetch_optional(pool)
        .await?;
    Ok(user)
}

/// Fetches the local user whose email is `email`, `None` when absent.
///
/// The comparison is case-insensitive because the column is citext (the
/// bind is cast, as in [`get_by_nickname`]); remote users, who have no
/// email, are never found.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_email(pool: &PgPool, email: &str) -> AppResult<Option<User>> {
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE email = $1::citext")
        .bind(email)
        .fetch_optional(pool)
        .await?;
    Ok(user)
}

/// The `password_reset` lookup: the local, active account whose nickname or
/// email is exactly `identifier` (citext folds case in both columns).
///
/// A remote mirror, a deactivated account and an account with no email to
/// write to are never a candidate — there is no reset letter to send them
/// (Pleroma's `Registration.password_reset` matches
/// `%User{local: true, is_active: true, email: email} when is_binary(email)`).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn find_password_reset_target(
    pool: &PgPool,
    identifier: &str,
) -> AppResult<Option<User>> {
    let user = sqlx::query_as::<_, User>(
        "SELECT * FROM users
          WHERE local AND is_active AND email IS NOT NULL
            AND (nickname = $1::citext OR email = $1::citext)
          LIMIT 1",
    )
    .bind(identifier)
    .fetch_optional(pool)
    .await?;
    Ok(user)
}

/// Fetches the user an `acct` handle names, `None` when absent.
///
/// The resolution mirrors `GET /api/v1/accounts/lookup` (Pleroma's
/// `AccountController.lookup`): the handle is first tried verbatim as a
/// stored nickname — case-insensitively, because the column is citext, so
/// `ALICE` and the remote handle `BOB@Remote.Example` find their rows just
/// like their stored spellings. When nothing matches and the handle
/// carries this instance's own domain (`alice@test.example`, compared
/// case-insensitively), the domain is stripped and the bare nickname is
/// tried again: local rows are stored without their domain (decision 7).
/// Resolving a handle this instance has never seen (via `WebFinger`)
/// belongs to the federation ticket.
///
/// This is a pure resolver: deactivated rows are found like any other;
/// callers that must hide them filter on [`is_active`](User::is_active)
/// themselves.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_by_acct(pool: &PgPool, config: &Config, acct: &str) -> AppResult<Option<User>> {
    if let Some(found) = get_by_nickname(pool, acct).await? {
        return Ok(Some(found));
    }
    let Some((nickname, domain)) = acct.split_once('@') else {
        return Ok(None);
    };
    if domain.eq_ignore_ascii_case(config.domain()) {
        return get_by_nickname(pool, nickname).await;
    }
    Ok(None)
}

/// Fetches the users whose ids are in `ids`, in `id` order (`UUIDv7` text
/// sorts by creation, decision 4). Ids with no row are skipped; an empty
/// `ids` yields an empty `Vec`.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn get_many(pool: &PgPool, ids: &[Uuid]) -> AppResult<Vec<User>> {
    let ids = ids.to_vec();
    let users = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ANY($1) ORDER BY id")
        .bind(&ids)
        .fetch_all(pool)
        .await?;
    Ok(users)
}

/// The order the profile directory walks its accounts in
/// (`GET /api/v1/directory`'s `order` parameter).
///
/// Which column the listing sorts on is the model's business; reading the
/// client's `active`/`new` text and defaulting an unrecognised value is the
/// endpoint's (Pleroma's `order_by_creation_date/2` matches `"new"` exactly
/// and falls back to the active order for anything else).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DirectoryOrder {
    /// Most recently active first: `last_status_at` descending, the accounts
    /// that never posted last (`DIRECTORY_ACTIVE_SQL`).
    #[default]
    Active,
    /// Newest account first: `inserted_at` descending
    /// (`DIRECTORY_NEW_SQL`).
    New,
}

impl DirectoryOrder {
    /// The `ORDER BY` clause of this order, with `id DESC` as tiebreak: two
    /// accounts sharing a timestamp must not land on both sides of an
    /// `OFFSET` boundary, which is how the directory pages.
    const fn order_by(self) -> &'static str {
        match self {
            Self::Active => DIRECTORY_ACTIVE_SQL,
            Self::New => DIRECTORY_NEW_SQL,
        }
    }
}

/// The shared head of [`directory_page`]: the accounts the directory may
/// answer with at all — active, and opted into discovery. The text stops
/// where the optional `local` filter belongs, because that is what
/// [`directory_page`] appends next; [`DIRECTORY_ACTIVE_SQL`] and
/// [`DIRECTORY_NEW_SQL`] carry the order, and the page's `LIMIT` and
/// `OFFSET` are binds the function pushes after them.
const DIRECTORY_SQL: &str = "SELECT * FROM users WHERE is_active AND discoverable";

/// The `active` order: newest `last_status_at` first, `NULL` (an account
/// that never posted) last, exactly Pleroma's `desc_nulls_last`.
const DIRECTORY_ACTIVE_SQL: &str = " ORDER BY last_status_at DESC NULLS LAST, id DESC";

/// The `new` order: newest account first.
const DIRECTORY_NEW_SQL: &str = " ORDER BY inserted_at DESC, id DESC";

/// One [`directory_page`] offset: the row to start at, `0` for the first
/// page — a directory has no pages before the first row.
const NO_OFFSET: i64 = 0;

/// The directory's discoverable, active accounts, `page`'s `limit` at a
/// time starting at its `offset`, in `order`'s order.
///
/// `local_only` is the caller's `local` flag: `true` restricts the listing
/// to accounts this instance owns (Pleroma's `exclude_remote`), `false`
/// leaves the remote mirrors in. Of [`PageParams`], only
/// [`limit`](PageParams::limit) and [`offset`](PageParams::offset) mean
/// anything here: a directory has no id cursor to walk, so
/// [`max_id`](PageParams::max_id) and its siblings are ignored, and
/// `offset` absent is the first page.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn directory_page(
    pool: &PgPool,
    order: DirectoryOrder,
    local_only: bool,
    page: PageParams,
) -> AppResult<Vec<User>> {
    let mut builder = QueryBuilder::<Postgres>::new(DIRECTORY_SQL);
    if local_only {
        let _local: &mut _ = builder.push(" AND local");
    }
    let _ordered: &mut _ = builder.push(order.order_by());
    let _limited: &mut _ = builder
        .push(" LIMIT ")
        .push_bind(page.limit)
        .push(" OFFSET ")
        .push_bind(page.offset.unwrap_or(NO_OFFSET));
    let users = builder.build_query_as::<User>().fetch_all(pool).await?;
    Ok(users)
}

/// The `SELECT` behind [`birthday_accounts`]: the `month`/`day` match is on
/// the calendar parts of the `birthday` column, so a `2000-02-29` birthday
/// lands only on a `2`/`29` query — a leap day, and no other day, ever
/// matches it (Pleroma's `birthday_month`/`birthday_day` filters of
/// `User.Query` compare the same two extracted parts). A `NULL` birthday
/// extracts to `NULL` and fails the comparison, so the explicit
/// `birthday IS NOT NULL` states what the comparison already implies.
const BIRTHDAYS_SQL: &str = "SELECT * FROM users
      WHERE is_active AND show_birthday AND birthday IS NOT NULL
        AND EXTRACT(MONTH FROM birthday)::int = $2
        AND EXTRACT(DAY FROM birthday)::int = $3
        AND (id = $1 OR id IN (
                 SELECT followee_id FROM follows
                  WHERE follower_id = $1 AND state = 'accepted'))
   ORDER BY id DESC";

/// The accounts whose birthday `GET /api/v1/pleroma/birthdays` answers for
/// `viewer_id`: the accounts it names.
///
/// That is the viewer itself and the accounts of their *accepted* follows,
/// each with [`show_birthday`](User::show_birthday) set and a
/// [`birthday`](User::birthday) falling on `month`/`day`, newest account
/// first.
///
/// A deactivated account is out (the flag the whole API hides suspended
/// rows behind), and a remote mirror is in: the follows of a local account
/// are local and remote alike.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn birthday_accounts(
    pool: &PgPool,
    viewer_id: Uuid,
    month: i32,
    day: i32,
) -> AppResult<Vec<User>> {
    let users = sqlx::query_as::<_, User>(BIRTHDAYS_SQL)
        .bind(viewer_id)
        .bind(month)
        .bind(day)
        .fetch_all(pool)
        .await?;
    Ok(users)
}

/// Adds `delta` to the denormalised counters of user `id` atomically,
/// clamping every count at zero, and refreshes `updated_at`.
///
/// The arithmetic happens in the `UPDATE` statement itself, so two
/// concurrent updates to the same row both take effect and the stored
/// counts are never negative. An unknown `id` matches no row and changes
/// nothing.
///
/// The step runs on any Postgres executor, so callers either fire it
/// against the pool or join it to a transaction of their own (see
/// [`remove_follower`](crate::models::follow::remove_follower), which
/// commits its row removal and its counter steps together).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn update_counts<'e, E>(executor: E, id: Uuid, delta: CountDelta) -> AppResult<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query(
        "UPDATE users
            SET followers_count = GREATEST(0, followers_count + $2),
                following_count = GREATEST(0, following_count + $3),
                statuses_count  = GREATEST(0, statuses_count + $4),
                updated_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(delta.followers)
    .bind(delta.following)
    .bind(delta.statuses)
    .execute(executor)
    .await
    .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`set_also_known_as`]: writes the whole alias
/// array in one statement and refreshes `updated_at`.
const SET_ALSO_KNOWN_AS_SQL: &str =
    "UPDATE users SET also_known_as = $2, updated_at = now() WHERE id = $1";

/// Stores `aliases` as user `id`'s `also_known_as` column, replacing what
/// was there.
///
/// The alias endpoints keep the add/remove logic (order-preserving append,
/// set-style removal) in the API layer and hand the whole new array in
/// here, so the column is written exactly as computed. Unknown ids match
/// no row and change nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_also_known_as(pool: &PgPool, id: Uuid, aliases: &[String]) -> AppResult<()> {
    sqlx::query(SET_ALSO_KNOWN_AS_SQL)
        .bind(id)
        .bind(aliases.to_vec())
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`mark_moved`]: records the move target and stamps
/// the cooldown timestamp in one statement, refreshing `updated_at`.
const MARK_MOVED_SQL: &str = "UPDATE users
      SET moved_to_ap_id = $2,
          last_move_at = now(),
          updated_at = now()
    WHERE id = $1";

/// Records that user `id` moved to the actor `target_ap_id`: writes
/// [`moved_to_ap_id`](User::moved_to_ap_id) and stamps
/// [`last_move_at`](User::last_move_at) with `now()`, the pair the move
/// endpoint's once-per-30-days cooldown reads back.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn mark_moved(pool: &PgPool, id: Uuid, target_ap_id: &str) -> AppResult<()> {
    sqlx::query(MARK_MOVED_SQL)
        .bind(id)
        .bind(target_ap_id)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`set_settings_store`]: writes the whole client
/// settings blob in one statement and refreshes `updated_at`.
const SET_SETTINGS_STORE_SQL: &str =
    "UPDATE users SET settings_store = $2, updated_at = now() WHERE id = $1";

/// Stores `store` as user `id`'s `settings_store` column, replacing what
/// was there.
///
/// The settings endpoints keep the merge logic (deep merge, `null`
/// removal) in the API layer and hand the whole new blob in here, so the
/// jsonb column is written exactly as computed — the same shape the
/// account entity round-trips. Unknown ids match no row and change
/// nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_settings_store(pool: &PgPool, id: Uuid, store: &Value) -> AppResult<()> {
    sqlx::query(SET_SETTINGS_STORE_SQL)
        .bind(id)
        .bind(store.clone())
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`set_preferred_frontend`]: writes the chosen
/// frontend and refreshes `updated_at`.
const SET_PREFERRED_FRONTEND_SQL: &str =
    "UPDATE users SET preferred_frontend = $2, updated_at = now() WHERE id = $1";

/// Stores `frontend` as user `id`'s `preferred_frontend` column: the
/// frontend the account picked at `PUT /api/v1/pleroma/preferred_frontend`
/// (a name the endpoint has validated against the configured list).
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_preferred_frontend(pool: &PgPool, id: Uuid, frontend: &str) -> AppResult<()> {
    sqlx::query(SET_PREFERRED_FRONTEND_SQL)
        .bind(id)
        .bind(frontend)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`set_email_notifications`]: writes the flag and
/// refreshes `updated_at`.
const SET_EMAIL_NOTIFICATIONS_SQL: &str =
    "UPDATE users SET email_notifications = $2, updated_at = now() WHERE id = $1";

/// Stores `enabled` as user `id`'s [`email_notifications`](User::email_notifications)
/// flag, replacing what was there.
///
/// This is the switch Pleroma's `User.switch_email_notifications`
/// (`lib/pleroma/user.ex`) moves, and the write the HTML unsubscribe page
/// (a later ticket) performs to clear it. Unknown ids match no row and
/// change nothing.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_email_notifications(pool: &PgPool, id: Uuid, enabled: bool) -> AppResult<()> {
    sqlx::query(SET_EMAIL_NOTIFICATIONS_SQL)
        .bind(id)
        .bind(enabled)
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`set_password`]: writes the new hash, clears the
/// pending-reset flag (the account has just proven mailbox control by
/// completing the flow) and refreshes `updated_at`.
const SET_PASSWORD_SQL: &str = "UPDATE users
           SET password_hash = $2,
               password_reset_pending = false,
               updated_at = now()
         WHERE id = $1";

/// Stores `password_hash` as user `id`'s `password_hash` and clears
/// [`password_reset_pending`](User::password_reset_pending): the write a
/// completed password reset performs (see
/// [`crate::models::password_reset`] and [`crate::api::password_reset`]).
///
/// Runs on the caller's executor (or any transaction), so the reset
/// endpoint can land the password, the token spend and the revocations as
/// one transaction.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_password<'e, E>(executor: E, id: Uuid, password_hash: &str) -> AppResult<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query(SET_PASSWORD_SQL)
        .bind(id)
        .bind(password_hash)
        .execute(executor)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`set_email`]: writes the new address and the
/// confirmed flag together and refreshes `updated_at`.
const SET_EMAIL_SQL: &str =
    "UPDATE users SET email = $2, email_confirmed = $3, updated_at = now() WHERE id = $1";

/// Stores `email` as user `id`'s [`email`](User::email) and
/// [`email_confirmed`](User::email_confirmed): the write
/// `POST /api/pleroma/change_email` performs.
///
/// The caller decides the flag — the address stays confirmed unless the
/// instance requires account confirmation (see
/// [`Config::account_activation_required`]).
///
/// [`Config::account_activation_required`]: crate::config::Config::account_activation_required
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_email(
    pool: &PgPool,
    id: Uuid,
    email: &str,
    email_confirmed: bool,
) -> AppResult<()> {
    sqlx::query(SET_EMAIL_SQL)
        .bind(id)
        .bind(email)
        .bind(Flag::new(email_confirmed))
        .execute(pool)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`set_activation`]: flips `is_active` and refreshes
/// `updated_at`, leaving every other column alone.
const SET_ACTIVATION_SQL: &str =
    "UPDATE users SET is_active = $2, updated_at = now() WHERE id = $1";

/// Sets user `id`'s [`is_active`](User::is_active) flag: the write
/// `POST /api/pleroma/disable_account` performs.
///
/// `false` is Pleroma's `User.set_activation(user, false)` — the account
/// stops signing in and the entity reads as suspended — and every other
/// column stays untouched: unlike [`mark_deleted`], this writes no
/// deletion time and queues no sweep.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn set_activation<'e, E>(executor: E, id: Uuid, active: bool) -> AppResult<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query(SET_ACTIVATION_SQL)
        .bind(id)
        .bind(Flag::new(active))
        .execute(executor)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`mark_deleted`]: deactivates the account and
/// stamps `deleted_at` in one statement, refreshing `updated_at`.
const MARK_DELETED_SQL: &str = "UPDATE users
       SET is_active = false,
           deleted_at = now(),
           updated_at = now()
     WHERE id = $1";

/// Deactivates user `id` and stamps [`deleted_at`](User::deleted_at): the
/// mark `POST /api/pleroma/delete_account` puts on the account.
///
/// The row itself stays — see [`purge_personal_data`] for the state its
/// [`users.delete`](crate::jobs::deletion::KIND_DELETE) job leaves behind.
///
/// Runs on the caller's executor (or any transaction), so the endpoint can
/// land the mark together with the token revocation.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn mark_deleted<'e, E>(executor: E, id: Uuid) -> AppResult<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query(MARK_DELETED_SQL)
        .bind(id)
        .execute(executor)
        .await
        .map(drop)?;
    Ok(())
}

/// The `UPDATE` behind [`purge_personal_data`]: nulls the two personal
/// columns, blanks the display name, and refreshes `updated_at`.
const PURGE_PERSONAL_DATA_SQL: &str = "UPDATE users
       SET email = NULL,
           password_hash = NULL,
           name = '',
           updated_at = now()
     WHERE id = $1";

/// Replaces user `id`'s personal data with empty values: the final step of
/// the [`users.delete`](crate::jobs::deletion::KIND_DELETE) job.
///
/// Email, password hash and display name are the account's person data —
/// the three go, the row stays (decision 7: other tables and mirrored
/// actors reference it), and nothing that was said about the account can
/// be attributed to a person any more.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn purge_personal_data<'e, E>(executor: E, id: Uuid) -> AppResult<()>
where
    E: Executor<'e, Database = Postgres>,
{
    sqlx::query(PURGE_PERSONAL_DATA_SQL)
        .bind(id)
        .execute(executor)
        .await
        .map(drop)?;
    Ok(())
}

/// The eleven Pleroma preference flags `update_credentials` writes: the
/// body of the `pleroma` block of the account entity, as the account
/// itself sets them.
///
/// Grouped out of [`ProfileFields`] because the endpoint merges them as one
/// set (every one is the same present-or-keep-the-stored-value boolean) and
/// because [`ProfileFields`] already carries the rest of the
/// profile.
#[derive(Clone, Copy, Debug)]
pub struct PleromaPrefs {
    /// Whether the follower collection is hidden from other accounts.
    pub hide_followers: Flag,
    /// Whether the follow collection is hidden from other accounts.
    pub hide_follows: Flag,
    /// Whether [`followers_count`](User::followers_count) renders as zero
    /// for everyone but the owner.
    pub hide_followers_count: Flag,
    /// Whether [`following_count`](User::following_count) renders as zero
    /// for everyone but the owner.
    pub hide_follows_count: Flag,
    /// Whether the favourites list is private to the account.
    pub hide_favorites: Flag,
    /// Whether the admin/moderator flags may be shown to other accounts.
    pub show_role: Flag,
    /// Whether this account's statuses skip thread containment.
    pub skip_thread_containment: Flag,
    /// Whether a `Move` of this account may take the following list with
    /// it.
    pub allow_following_move: Flag,
    /// Whether the account accepts chat messages.
    pub accepts_chat_messages: Flag,
    /// Whether the account asked clients not to render rich text.
    pub no_rich_text: Flag,
    /// Whether strangers may see [`birthday`](ProfileFields::birthday).
    pub show_birthday: Flag,
}

/// The whole `PATCH /api/v1/accounts/update_credentials` write: every
/// profile column the endpoint owns.
///
/// The text and images of the Mastodon profile, the posting defaults of the
/// `source` block, the eleven `pleroma` flags, the birthday and the client
/// settings store.
///
/// Every one of them arrives already merged with the stored row by the
/// caller, so fields absent from the request are rewritten with the value
/// they already hold and change nothing.
#[derive(Debug)]
pub struct ProfileFields {
    /// The display name to store, sanitized by the caller.
    pub name: String,
    /// The profile note as rendered HTML for the `bio` column.
    pub bio: String,
    /// The profile note as its source text for the `raw_bio` column.
    pub raw_bio: String,
    /// Whether follow requests need manual approval.
    pub locked: Flag,
    /// Whether the account is flagged as automated.
    pub bot: Flag,
    /// Whether the account opted into discovery.
    pub discoverable: Flag,
    /// The profile metadata as rendered HTML entries for the `fields`
    /// column: a JSON array of `{name, value}` objects.
    pub fields: Value,
    /// The profile metadata as its source text for the `raw_fields`
    /// column: the same entries as [`fields`](Self::fields), with the
    /// values and names exactly as typed.
    pub raw_fields: Value,
    /// The avatar URL for the `avatar_url` column: the stored one when the
    /// body carried no `avatar` parameter, the public URL of a newly stored
    /// upload when it carried a file, `None` when it cleared the image.
    pub avatar_url: Option<String>,
    /// The banner URL for the `banner_url` column, the three states of
    /// [`avatar_url`](Self::avatar_url) read from the body's `header`.
    pub banner_url: Option<String>,
    /// The background URL for the `background_image_url` column, the three
    /// states of [`avatar_url`](Self::avatar_url) read from the body's
    /// `pleroma_background_image`.
    pub background_image_url: Option<String>,
    /// The visibility new statuses default to, for the `default_scope`
    /// column (the entity's `source.privacy`): one of `public`, `unlisted`,
    /// `private` or `direct`, validated by the caller.
    pub default_scope: String,
    /// Whether new statuses are flagged sensitive by default, for the
    /// `sensitive_default` column (the entity's `source.sensitive`).
    pub sensitive_default: Flag,
    /// The BCP 47 language tag new statuses default to, for the `language`
    /// column (the entity's `source.language`); `None` stores `NULL`, the
    /// account stating no preference.
    pub language: Option<String>,
    /// The eleven `pleroma` preference flags of [`PleromaPrefs`], one column
    /// each.
    pub pleroma: PleromaPrefs,
    /// The account's birthday for the `birthday` column; `None` stores
    /// `NULL`, which is how the endpoint clears the date.
    pub birthday: Option<NaiveDate>,
    /// The whole client settings blob for the `settings_store` column: the
    /// endpoint merges the submitted object over the stored one and the
    /// write stores the result.
    pub settings_store: Value,
    /// The `ActivityPub` actor kind for the `actor_type` column: `Person` or
    /// `Service`, the pair the endpoint keeps in step with [`bot`](Self::bot).
    pub actor_type: String,
}

/// The `UPDATE` behind [`update_profile`]: writes the profile columns in one
/// statement, refreshes `updated_at` and returns the stored row.
const UPDATE_PROFILE_SQL: &str = "UPDATE users
      SET name = $2,
          bio = $3,
          raw_bio = $4,
          locked = $5,
          bot = $6,
          discoverable = $7,
          fields = $8,
          raw_fields = $9,
          avatar_url = $10,
          banner_url = $11,
          background_image_url = $12,
          default_scope = $13,
          sensitive_default = $14,
          language = $15,
          actor_type = $16,
          birthday = $17,
          settings_store = $18,
          hide_followers = $19,
          hide_follows = $20,
          hide_followers_count = $21,
          hide_follows_count = $22,
          hide_favorites = $23,
          show_role = $24,
          skip_thread_containment = $25,
          allow_following_move = $26,
          accepts_chat_messages = $27,
          no_rich_text = $28,
          show_birthday = $29,
          updated_at = now()
     WHERE id = $1
  RETURNING *";

/// Stores `fields` as user `id`'s profile columns and returns the updated
/// row.
///
/// `None` when `id` names no row (the caller authenticated it moments
/// earlier, so that only happens if the account was deleted mid-request).
/// The endpoint layer computes the merged values — this function writes
/// exactly what it is handed, the same split the alias and
/// settings-store writers use.
///
/// # Errors
///
/// Any database failure (connection, encoding) arrives as
/// [`AppError::Database`](crate::error::AppError).
pub async fn update_profile(
    pool: &PgPool,
    id: Uuid,
    fields: &ProfileFields,
) -> AppResult<Option<User>> {
    Ok(sqlx::query_as::<_, User>(UPDATE_PROFILE_SQL)
        .bind(id)
        .bind(&fields.name)
        .bind(&fields.bio)
        .bind(&fields.raw_bio)
        .bind(fields.locked)
        .bind(fields.bot)
        .bind(fields.discoverable)
        .bind(fields.fields.clone())
        .bind(fields.raw_fields.clone())
        .bind(fields.avatar_url.as_deref())
        .bind(fields.banner_url.as_deref())
        .bind(fields.background_image_url.as_deref())
        .bind(fields.default_scope.as_str())
        .bind(fields.sensitive_default)
        .bind(fields.language.as_deref())
        .bind(fields.actor_type.as_str())
        .bind(fields.birthday)
        .bind(fields.settings_store.clone())
        .bind(fields.pleroma.hide_followers)
        .bind(fields.pleroma.hide_follows)
        .bind(fields.pleroma.hide_followers_count)
        .bind(fields.pleroma.hide_follows_count)
        .bind(fields.pleroma.hide_favorites)
        .bind(fields.pleroma.show_role)
        .bind(fields.pleroma.skip_thread_containment)
        .bind(fields.pleroma.allow_following_move)
        .bind(fields.pleroma.accepts_chat_messages)
        .bind(fields.pleroma.no_rich_text)
        .bind(fields.pleroma.show_birthday)
        .fetch_optional(pool)
        .await?)
}

/// The `ActivityPub` actor id of the local account `nickname` on the
/// instance served at `base_url`: `{base_url}/users/{nickname}`.
#[must_use]
pub fn local_ap_id(base_url: &str, nickname: &str) -> String {
    format!("{base_url}/users/{nickname}")
}

/// The inbox endpoint of actor `ap_id`: `{ap_id}/inbox`.
#[must_use]
pub fn inbox(ap_id: &str) -> String {
    format!("{ap_id}/inbox")
}

/// The outbox collection of actor `ap_id`: `{ap_id}/outbox`.
#[must_use]
pub fn outbox(ap_id: &str) -> String {
    format!("{ap_id}/outbox")
}

/// The followers collection of actor `ap_id`: `{ap_id}/followers`.
#[must_use]
pub fn followers(ap_id: &str) -> String {
    format!("{ap_id}/followers")
}

/// The following collection of actor `ap_id`: `{ap_id}/following`.
#[must_use]
pub fn following(ap_id: &str) -> String {
    format!("{ap_id}/following")
}

/// The featured (pinned) collection of actor `ap_id`: `{ap_id}/featured`.
#[must_use]
pub fn featured(ap_id: &str) -> String {
    format!("{ap_id}/featured")
}

#[cfg(test)]
mod tests {
    use chrono::SubsecRound as _;
    use sqlx::{Error as SqlxError, PgPool};

    use super::*;
    use crate::error::AppError;

    /// The instance origin the fixtures pretend to serve.
    const BASE: &str = "https://test.example";
    /// A password hash that must never surface in `Debug` output.
    const SECRET_HASH: &str = "$argon2id$v=19$secret-digest";
    /// Private key material that must never surface in `Debug` output.
    const SECRET_KEY: &str = "BEGIN RSA PRIVATE KEY secret END";
    /// A TOTP secret (inside `mfa_settings`) that must never surface in
    /// `Debug` output.
    const SECRET_TOTP: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
    /// An email-confirmation token: a link credential, so it must never
    /// surface in `Debug` output either.
    const SECRET_CONFIRMATION: &str = "confirmation-token-c1JCzoTmgLmq9B1z7";

    /// A local [`NewUser`] named `nickname` at this module's [`BASE`] url.
    fn local_new(nickname: &str) -> NewUser {
        NewUser::local(nickname, &local_ap_id(BASE, nickname))
    }

    /// Reads the row behind `id` back out of the table, panicking if absent.
    async fn fetch(pool: &PgPool, id: Uuid) -> User {
        get_by_id(pool, id).await.unwrap().expect("row exists")
    }

    /// Every column of the fixture `NewUser` that came back on the inserted
    /// row, plus the defaults the model and the database fill in.
    fn assert_stored_columns(user: &User) {
        assert!(user.id.get_version_num() == 7, "id is a UUIDv7");
        assert_eq!(user.nickname, "alice", "the nickname round trips");
        assert_eq!(user.ap_id, format!("{BASE}/users/alice"), "ap_id");
        assert!(user.local.get(), "the local flag round trips");
        assert_eq!(user.actor_type, DEFAULT_ACTOR_TYPE, "constructor default");
        assert_eq!(
            user.email.as_deref(),
            Some("alice@example.org"),
            "email round trips"
        );
        assert_eq!(
            user.password_hash.as_deref(),
            Some(SECRET_HASH),
            "hash is stored even though Debug redacts it"
        );
        assert_eq!(user.name, "Alice", "display name round trips");
        assert_eq!(user.bio, "<p>hi</p>", "sanitized note round trips");
        assert_eq!(user.raw_bio, "hi", "source note round trips");
        assert_eq!(
            user.avatar_url.as_deref(),
            Some("https://test.example/avatar.png"),
            "avatar url"
        );
        assert_eq!(
            user.banner_url.as_deref(),
            Some("https://test.example/banner.png"),
            "banner url"
        );
        assert_eq!(
            user.profile_url.as_deref(),
            Some("https://test.example/users/alice"),
            "profile url"
        );
        assert!(
            !user.locked.get() && !user.bot.get() && !user.discoverable.get(),
            "the three option flags default to false"
        );
        assert_eq!(
            user.fields,
            json!([{"name": "site", "value": "<a href=\"https://alice.example\">alice.example</a>"}]),
            "fields round trip through jsonb"
        );
        assert_eq!(
            user.raw_fields,
            json!([{"name": "site", "value": "https://alice.example"}]),
            "raw fields round trip through jsonb"
        );
        assert_eq!(
            user.inbox.as_deref(),
            Some("https://test.example/users/alice/inbox"),
            "inbox column"
        );
        assert_eq!(user.public_key.as_deref(), Some("PUBLIC-PEM"), "public key");
        assert_eq!(
            user.private_key.as_deref(),
            Some(SECRET_KEY),
            "private key is stored even though Debug redacts it"
        );
        assert!(
            !user.is_admin.get() && !user.is_moderator.get(),
            "roles default to false"
        );
        assert!(user.is_active.get(), "a fresh account is active");
        assert_eq!(
            (
                user.followers_count,
                user.following_count,
                user.statuses_count
            ),
            (0, 0, 0),
            "counts default to zero"
        );
        assert!(
            user.inserted_at <= user.updated_at,
            "the database stamps both timestamps"
        );
    }

    /// The four reads of [`crate::models::user`] all find the inserted row,
    /// and two of them fold case the way `citext` does.
    async fn assert_every_lookup_finds(pool: &PgPool, user: &User) {
        let by_id = get_by_id(pool, user.id).await.unwrap().expect("by id");
        let by_nickname = get_by_nickname(pool, "ALiCE")
            .await
            .unwrap()
            .expect("citext nickname lookup folds case");
        let by_ap_id = get_by_ap_id(pool, &user.ap_id)
            .await
            .unwrap()
            .expect("by ap_id");
        let by_email = get_by_email(pool, "ALICE@example.org")
            .await
            .unwrap()
            .expect("citext email lookup folds case");
        assert_eq!(by_nickname.id, user.id, "the nickname finds the row");
        assert_eq!(by_ap_id.id, user.id, "the actor id finds the row");
        assert_eq!(by_email.id, user.id, "the email finds the row");
        assert_eq!(
            by_id.nickname, by_nickname.nickname,
            "every path reads the same row"
        );
        let many = get_many(pool, &[user.id, new_id()]).await.unwrap();
        assert_eq!(many.len(), 1, "unknown ids are simply absent");
        assert_eq!(
            many.first().map(|found| found.id),
            Some(user.id),
            "the known id comes back"
        );
    }

    /// The reads that find nothing: an empty id list, and an unknown actor id,
    /// email or nickname.
    async fn assert_unknown_lookups_answer_none(pool: &PgPool) {
        assert!(
            get_many(pool, &[]).await.unwrap().is_empty(),
            "no ids, no rows"
        );
        assert!(
            get_by_ap_id(pool, "https://nowhere.example/users/x")
                .await
                .unwrap()
                .is_none(),
            "an unknown ap_id is None"
        );
        assert!(
            get_by_email(pool, "nobody@example.org")
                .await
                .unwrap()
                .is_none(),
            "an unknown email is None"
        );
        assert!(
            get_by_nickname(pool, "nobody").await.unwrap().is_none(),
            "an unknown nickname is None"
        );
    }

    /// The local-user fixture of these tests: every column the model can
    /// write, set to something distinct.
    async fn inserted_alice(pool: &PgPool) -> User {
        let fetched_at = Utc::now().trunc_subsecs(6);
        let new = NewUser {
            email: Some("alice@example.org".to_owned()),
            password_hash: Some(SECRET_HASH.to_owned()),
            name: "Alice".to_owned(),
            bio: "<p>hi</p>".to_owned(),
            raw_bio: "hi".to_owned(),
            avatar_url: Some(format!("{BASE}/avatar.png")),
            banner_url: Some(format!("{BASE}/banner.png")),
            profile_url: Some(local_ap_id(BASE, "alice")),
            fields: json!([{"name": "site", "value": "<a href=\"https://alice.example\">alice.example</a>"}]),
            raw_fields: json!([{"name": "site", "value": "https://alice.example"}]),
            inbox: Some(inbox(&local_ap_id(BASE, "alice"))),
            public_key: Some("PUBLIC-PEM".to_owned()),
            private_key: Some(SECRET_KEY.to_owned()),
            last_fetched_at: Some(fetched_at),
            ..local_new("alice")
        };
        let user = insert(pool, &new).await.unwrap();
        assert_eq!(
            user.last_fetched_at,
            Some(fetched_at),
            "timestamptz round trips"
        );
        assert!(user.domain.is_none(), "local users carry no domain");
        assert!(user.shared_inbox.is_none(), "unset endpoints stay NULL");
        assert!(user.last_status_at.is_none(), "no statuses yet");
        user
    }

    #[tokio::test]
    async fn insert_round_trips_every_column() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = inserted_alice(&pool).await;

        assert_stored_columns(&user);
        assert_every_lookup_finds(&pool, &user).await;
        assert_unknown_lookups_answer_none(&pool).await;
    }

    /// The privacy and behaviour flags a plain [`NewUser::local`] row gets
    /// from the migration.
    fn assert_pleroma_flag_defaults(user: &User) {
        assert!(
            !user.hide_followers.get() && !user.hide_follows.get(),
            "the follow collections are public by default"
        );
        assert!(
            !user.hide_followers_count.get() && !user.hide_follows_count.get(),
            "and so are both counters"
        );
        assert!(
            user.hide_favorites.get(),
            "hide_favorites is the one privacy flag that defaults to true"
        );
        assert!(
            user.allow_following_move.get(),
            "a move takes the following list unless the account says otherwise"
        );
        assert!(
            !user.skip_thread_containment.get(),
            "thread containment applies by default"
        );
        assert!(
            user.accepts_chat_messages.get(),
            "chat is accepted unless the account opts out"
        );
        assert!(user.show_role.get(), "roles are shown by default");
        assert!(
            !user.no_rich_text.get(),
            "rich text is on unless the account asks for plain text"
        );
        assert!(
            !user.sensitive_default.get(),
            "new statuses are not flagged sensitive by default"
        );
        assert!(
            !user.is_suggested.get(),
            "the suggestion pool is admin-curated, so nothing starts in it"
        );
        assert!(
            user.email_notifications.get(),
            "accounts start opted in to email"
        );
    }

    /// The rest of the Pleroma extension columns of a plain
    /// [`NewUser::local`] row: the empty collections and blobs, the unset
    /// images and moves, and the account-state stamps.
    fn assert_pleroma_value_defaults(user: &User) {
        assert_eq!(
            user.default_scope, DEFAULT_SCOPE,
            "statuses default to public"
        );
        assert!(
            user.language.is_none(),
            "an account that named no language has no default"
        );
        assert!(user.tags.is_empty(), "tags default to the empty array");
        assert!(
            user.also_known_as.is_empty(),
            "aliases default to the empty array"
        );
        assert_eq!(
            user.settings_store,
            json!({}),
            "the settings blob defaults to an empty object"
        );
        assert!(
            user.background_image_url.is_none(),
            "there is no default background image"
        );
        assert!(user.birthday.is_none(), "no birthday is stored by default");
        assert!(
            !user.show_birthday.get(),
            "a birthday stays private by default"
        );
        assert!(user.moved_to_ap_id.is_none(), "nobody has moved by default");
        assert!(
            user.last_move_at.is_none(),
            "an account that never moved has no move stamp"
        );
        assert!(
            user.email_confirmed.get(),
            "email_confirmed defaults to true: an account is not pending \
             confirmation until the confirmation flow says otherwise"
        );
        assert!(
            !user.approval_pending.get(),
            "approval is not pending by default"
        );
        assert!(
            user.confirmation_token.is_none(),
            "no confirmation token exists until the confirmation flow mints one"
        );
        assert!(
            user.confirmation_sent_at.is_none(),
            "no confirmation letter was queued by default"
        );
        assert!(
            user.registration_reason.is_none(),
            "an account that stated no registration reason has none stored"
        );
        assert!(
            user.last_active_at.is_none(),
            "an account that never logged in has no last-active time"
        );
        assert_eq!(
            user.mfa_settings,
            json!({"enabled": false, "totp": {"secret": null, "confirmed": false}, "backup_codes": []}),
            "mfa_settings defaults to the migration's all-off blob"
        );
        assert_eq!(
            user.notification_settings,
            notification_settings::NotificationSettings::default_value(),
            "notification_settings defaults to the migration's all-off blob"
        );
        assert!(
            user.preferred_frontend.is_none(),
            "an account that never picked a frontend has no preference"
        );
    }

    /// Every Pleroma extension column at the default the migration states,
    /// for a row inserted through the plain [`NewUser::local`] constructor
    /// (which mirrors those defaults).
    #[tokio::test]
    async fn pleroma_columns_default_as_the_migration_states() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = insert(&pool, &local_new("cara")).await.unwrap();

        assert_pleroma_flag_defaults(&user);
        assert_pleroma_value_defaults(&user);
    }

    /// The Pleroma extension flags, [`NewUser::default_scope`] and the
    /// sensitive default, each set to the opposite of its migration default.
    fn assert_pleroma_flags_round_trip(user: &User) {
        assert!(
            user.hide_followers.get()
                && user.hide_follows.get()
                && user.hide_followers_count.get()
                && user.hide_follows_count.get(),
            "the four hiding flags round trip as set"
        );
        assert!(
            !user.hide_favorites.get(),
            "so does the one defaulting to true, flipped"
        );
        assert!(
            !user.allow_following_move.get()
                && user.skip_thread_containment.get()
                && !user.accepts_chat_messages.get()
                && !user.show_role.get()
                && user.no_rich_text.get(),
            "behaviour and role flags round trip"
        );
        assert_eq!(
            user.default_scope, "private",
            "the default scope round trips"
        );
        assert!(
            user.sensitive_default.get(),
            "the sensitive default round trips through its flag"
        );
        assert!(user.show_birthday.get(), "the birthday switch round trips");
        assert!(
            !user.email_confirmed.get(),
            "an unconfirmed email round trips"
        );
        assert!(user.approval_pending.get(), "pending approval round trips");
        assert!(
            !user.email_notifications.get(),
            "the email switch round trips flipped"
        );
    }

    /// The Pleroma extension values: both arrays in the order they were
    /// written, both jsonb blobs, the date, the move and the two stamps.
    fn assert_pleroma_values_round_trip(user: &User, active_at: DateTime<Utc>) {
        assert_eq!(
            user.language.as_deref(),
            Some("eo"),
            "the default language round trips"
        );
        assert_eq!(
            user.tags,
            vec!["gnw".to_owned(), "rust".to_owned()],
            "a text[] keeps its order"
        );
        assert_eq!(
            user.also_known_as,
            vec![local_ap_id(BASE, "alicia"), local_ap_id(BASE, "alice2")],
            "the alias array keeps its order"
        );
        assert_eq!(
            user.settings_store,
            json!({"pleroma_fe": {"theme": "dark"}}),
            "the settings blob round trips through jsonb"
        );
        assert_eq!(
            user.background_image_url.as_deref(),
            Some("https://test.example/bg.png"),
            "the background url round trips"
        );
        assert_eq!(
            user.birthday,
            NaiveDate::from_ymd_opt(1990, 1, 31),
            "a date column round trips"
        );
        assert_eq!(
            user.moved_to_ap_id.as_deref(),
            Some("https://other.example/users/alice"),
            "the move target round trips"
        );
        assert_eq!(
            user.last_move_at,
            Some(active_at),
            "the last-move timestamp round trips"
        );
        assert_eq!(
            user.last_active_at,
            Some(active_at),
            "the last-active timestamp round trips"
        );
        assert_eq!(
            user.confirmation_token.as_deref(),
            Some(SECRET_CONFIRMATION),
            "the confirmation token round trips even though Debug redacts it"
        );
        assert_eq!(
            user.confirmation_sent_at,
            Some(active_at),
            "the confirmation send stamp round trips"
        );
        assert_eq!(
            user.mfa_settings,
            json!({
                "enabled": true,
                "totp": {"secret": "BASE32SECRET", "confirmed": true},
                "backup_codes": ["0123456789abcdef"],
            }),
            "the mfa_settings blob round trips through jsonb"
        );
        assert_eq!(
            user.preferred_frontend.as_deref(),
            Some("pleroma-fe"),
            "the chosen frontend round trips through the text column"
        );
    }

    /// The Pleroma extension columns round trip through [`insert`]: arrays
    /// keep their order, the jsonb blob keeps its shape, the date and the
    /// timestamp come back as stored, and every flag keeps its value.
    #[tokio::test]
    async fn pleroma_columns_round_trip() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let active_at = Utc::now().trunc_subsecs(6);
        let new = NewUser {
            hide_followers: Flag::new(true),
            hide_follows: Flag::new(true),
            hide_followers_count: Flag::new(true),
            hide_follows_count: Flag::new(true),
            hide_favorites: Flag::new(false),
            allow_following_move: Flag::new(false),
            skip_thread_containment: Flag::new(true),
            accepts_chat_messages: Flag::new(false),
            show_role: Flag::new(false),
            no_rich_text: Flag::new(true),
            default_scope: "private".to_owned(),
            sensitive_default: Flag::new(true),
            language: Some("eo".to_owned()),
            tags: vec!["gnw".to_owned(), "rust".to_owned()],
            settings_store: json!({"pleroma_fe": {"theme": "dark"}}),
            background_image_url: Some(format!("{BASE}/bg.png")),
            birthday: NaiveDate::from_ymd_opt(1990, 1, 31),
            show_birthday: Flag::new(true),
            also_known_as: vec![local_ap_id(BASE, "alicia"), local_ap_id(BASE, "alice2")],
            moved_to_ap_id: Some("https://other.example/users/alice".to_owned()),
            last_move_at: Some(active_at),
            email_confirmed: Flag::new(false),
            approval_pending: Flag::new(true),
            confirmation_token: Some(SECRET_CONFIRMATION.to_owned()),
            confirmation_sent_at: Some(active_at),
            registration_reason: Some("please let me in".to_owned()),
            last_active_at: Some(active_at),
            mfa_settings: json!({
                "enabled": true,
                "totp": {"secret": "BASE32SECRET", "confirmed": true},
                "backup_codes": ["0123456789abcdef"],
            }),
            preferred_frontend: Some("pleroma-fe".to_owned()),
            email_notifications: Flag::new(false),
            ..local_new("dora")
        };
        let user = insert(&pool, &new).await.unwrap();
        assert_pleroma_flags_round_trip(&user);
        assert_pleroma_values_round_trip(&user, active_at);
        assert_eq!(
            user.registration_reason.as_deref(),
            Some("please let me in"),
            "the registration reason round trips"
        );
    }

    #[tokio::test]
    async fn duplicate_nickname_in_other_case_is_rejected() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("Alice")).await.unwrap();
        let err = insert(&pool, &local_new("aLiCe")).await.unwrap_err();
        let AppError::Database(SqlxError::Database(db_err)) = err else {
            panic!("a nickname collision must surface as a database error")
        };
        assert_eq!(
            db_err.code().as_deref(),
            Some("23505"),
            "the duplicate is the citext unique constraint, not some other failure"
        );

        let rows = get_many(&pool, &[alice.id]).await.unwrap();
        assert_eq!(rows.len(), 1, "the rejected insert stored nothing");
        let stored = get_by_nickname(&pool, "aLiCe")
            .await
            .unwrap()
            .expect("citext lookup folds case");
        assert_eq!(stored.id, alice.id, "the first spelling owns the row");
        assert_eq!(stored.nickname, "Alice", "the stored case survives citext");
    }

    #[tokio::test]
    async fn remote_user_nickname_carries_the_host() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let ap_id = "https://remote.example/users/bob";
        let new = NewUser {
            nickname: "bob@remote.example".to_owned(),
            ap_id: ap_id.to_owned(),
            domain: Some("remote.example".to_owned()),
            name: "Bob".to_owned(),
            inbox: Some(inbox(ap_id)),
            shared_inbox: Some("https://remote.example/inbox".to_owned()),
            outbox: Some(outbox(ap_id)),
            followers_url: Some(followers(ap_id)),
            following_url: Some(following(ap_id)),
            featured_url: Some(featured(ap_id)),
            public_key: Some("REMOTE-PEM".to_owned()),
            discoverable: Flag::new(true),
            followers_count: 12,
            following_count: 3,
            statuses_count: 48,
            last_fetched_at: Some(Utc::now()),
            ..NewUser::default()
        };
        let user = insert(&pool, &new).await.unwrap();
        assert!(!user.local.get(), "the default NewUser is a remote mirror");
        assert_eq!(
            user.nickname, "bob@remote.example",
            "the user@host handle round trips"
        );
        assert_eq!(user.domain.as_deref(), Some("remote.example"), "domain");
        assert_eq!(user.inbox.as_deref(), Some(inbox(ap_id).as_str()), "inbox");
        assert_eq!(
            user.shared_inbox.as_deref(),
            Some("https://remote.example/inbox"),
            "shared inbox"
        );
        assert_eq!(
            user.outbox.as_deref(),
            Some(outbox(ap_id).as_str()),
            "outbox"
        );
        assert_eq!(
            user.followers_url.as_deref(),
            Some(followers(ap_id).as_str()),
            "followers collection"
        );
        assert_eq!(
            user.following_url.as_deref(),
            Some(following(ap_id).as_str()),
            "following collection"
        );
        assert_eq!(
            user.featured_url.as_deref(),
            Some(featured(ap_id).as_str()),
            "featured collection"
        );
        assert!(
            user.private_key.is_none(),
            "remote users keep no private key"
        );
        assert!(user.email.is_none(), "remote users keep no email");
        assert!(user.discoverable.get(), "flags round trip");
        assert_eq!(
            (
                user.followers_count,
                user.following_count,
                user.statuses_count
            ),
            (12, 3, 48),
            "fetched counts are stored as given"
        );

        let fetched = get_by_nickname(&pool, "bob@remote.example")
            .await
            .unwrap()
            .expect("the remote handle finds the row");
        assert_eq!(fetched.ap_id, ap_id, "same row by nickname");
    }

    #[tokio::test]
    async fn get_by_acct_folds_case_and_strips_the_local_domain() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let config = Config::for_tests();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        let bob = insert(
            &pool,
            &NewUser {
                nickname: "bob@remote.example".to_owned(),
                ap_id: "https://remote.example/users/bob".to_owned(),
                domain: Some("remote.example".to_owned()),
                ..NewUser::default()
            },
        )
        .await
        .unwrap();

        for (acct, expected) in [
            ("alice", Some(alice.id)),
            ("ALICE", Some(alice.id)),
            ("alice@test.example", Some(alice.id)),
            ("AlIce@TEST.example", Some(alice.id)),
            ("bob@remote.example", Some(bob.id)),
            ("BOB@Remote.Example", Some(bob.id)),
            ("nobody", None),
            ("nobody@test.example", None),
            ("alice@other.example", None),
            ("carol@remote.example", None),
            ("@test.example", None),
        ] {
            let found = get_by_acct(&pool, &config, acct).await.unwrap();
            let got = found.map(|user| user.id);
            assert_eq!(
                got, expected,
                "`{acct}` must resolve to {expected:?}, got {got:?}"
            );
        }
    }

    #[tokio::test]
    async fn update_counts_adds_atomically_and_clamps_at_zero() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = insert(&pool, &local_new("dave")).await.unwrap();
        update_counts(
            &pool,
            user.id,
            CountDelta {
                followers: 4,
                following: 2,
                statuses: 7,
            },
        )
        .await
        .unwrap();

        // Two concurrent +1s must both land: the arithmetic is in the
        // UPDATE itself, not a read-modify-write.
        let plus_one = CountDelta {
            followers: 1,
            following: 0,
            statuses: 0,
        };
        let (left, right) = tokio::join!(
            update_counts(&pool, user.id, plus_one),
            update_counts(&pool, user.id, plus_one)
        );
        left.unwrap();
        right.unwrap();

        let after = fetch(&pool, user.id).await;
        assert_eq!(after.followers_count, 6, "both concurrent steps landed");
        assert_eq!(after.following_count, 2, "the untouched step stays");
        assert_eq!(after.statuses_count, 7, "the untouched step stays");

        update_counts(
            &pool,
            user.id,
            CountDelta {
                followers: -100,
                following: -100,
                statuses: -100,
            },
        )
        .await
        .unwrap();
        let clamped = fetch(&pool, user.id).await;
        assert_eq!(
            (
                clamped.followers_count,
                clamped.following_count,
                clamped.statuses_count
            ),
            (0, 0, 0),
            "counts never go negative"
        );

        // An unknown id is a no-op, not an error.
        update_counts(&pool, new_id(), CountDelta::default())
            .await
            .unwrap();
        let still = fetch(&pool, user.id).await;
        assert_eq!(still.followers_count, 0, "a no-op update changed nothing");
    }

    #[tokio::test]
    async fn debug_redacts_secrets_in_user_and_new_user() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = insert(
            &pool,
            &NewUser {
                password_hash: Some(SECRET_HASH.to_owned()),
                private_key: Some(SECRET_KEY.to_owned()),
                confirmation_token: Some(SECRET_CONFIRMATION.to_owned()),
                mfa_settings: json!({
                    "enabled": true,
                    "totp": {"secret": SECRET_TOTP, "confirmed": true},
                    "backup_codes": [],
                }),
                ..local_new("erin")
            },
        )
        .await
        .unwrap();
        let debug = format!("{user:?}");
        assert!(
            !debug.contains(SECRET_HASH),
            "the password hash must not leak"
        );
        assert!(!debug.contains(SECRET_KEY), "the private key must not leak");
        assert!(
            !debug.contains(SECRET_TOTP),
            "the TOTP secret inside mfa_settings must not leak"
        );
        assert!(
            !debug.contains(SECRET_CONFIRMATION),
            "the confirmation token must not leak: {debug}"
        );
        assert!(
            debug.contains(r#"password_hash: "<redacted>""#),
            "the redaction marker names the field"
        );
        assert!(
            debug.contains(r#"private_key: "<redacted>""#),
            "the redaction marker names the field"
        );
        assert!(
            debug.contains(r#"confirmation_token: "<redacted>""#),
            "the confirmation token keeps its own marker"
        );
        assert!(
            debug.contains(r#"mfa_settings: "<redacted>""#),
            "the MFA blob is redacted whole, secret included"
        );
        assert!(debug.contains("erin"), "the rest of the row is visible");

        let new = NewUser {
            password_hash: Some(SECRET_HASH.to_owned()),
            private_key: Some(SECRET_KEY.to_owned()),
            confirmation_token: Some(SECRET_CONFIRMATION.to_owned()),
            mfa_settings: json!({
                "enabled": true,
                "totp": {"secret": SECRET_TOTP, "confirmed": true},
                "backup_codes": [],
            }),
            ..local_new("erin")
        };
        let new_debug = format!("{new:?}");
        assert!(
            !new_debug.contains(SECRET_HASH)
                && !new_debug.contains(SECRET_KEY)
                && !new_debug.contains(SECRET_TOTP)
                && !new_debug.contains(SECRET_CONFIRMATION),
            "NewUser redacts the same secrets"
        );
        assert!(
            new_debug.contains(r#"password_hash: "<redacted>""#),
            "and shows the markers"
        );
    }

    /// The shared exclusion fragment must name exactly the instance actor
    /// nickname, or the listing queries and the service would drift apart.
    #[test]
    fn the_instance_actor_exclusion_names_the_instance_actor() {
        assert!(
            NOT_INSTANCE_ACTOR.contains(INSTANCE_ACTOR_NICKNAME),
            "the SQL fragment excludes the account the constant names"
        );
    }

    #[test]
    fn url_helpers_append_the_activitypub_paths() {
        let alice = local_ap_id(BASE, "alice");
        assert_eq!(
            alice, "https://test.example/users/alice",
            "local ap_id path"
        );
        assert_eq!(
            inbox(&alice),
            "https://test.example/users/alice/inbox",
            "inbox"
        );
        assert_eq!(
            outbox(&alice),
            "https://test.example/users/alice/outbox",
            "outbox"
        );
        assert_eq!(
            followers(&alice),
            "https://test.example/users/alice/followers",
            "followers"
        );
        assert_eq!(
            following(&alice),
            "https://test.example/users/alice/following",
            "following"
        );
        assert_eq!(
            featured(&alice),
            "https://test.example/users/alice/featured",
            "featured"
        );
        assert_eq!(
            inbox("https://remote.example/users/bob"),
            "https://remote.example/users/bob/inbox",
            "works on remote ids too"
        );
    }

    #[test]
    fn local_constructor_uses_the_column_defaults() {
        let new = local_new("alice");
        assert!(new.local.get(), "the constructor marks the account local");
        assert_eq!(new.nickname, "alice", "nickname");
        assert_eq!(new.ap_id, "https://test.example/users/alice", "ap_id");
        assert!(new.domain.is_none(), "no domain for a local account");
        assert_eq!(new.actor_type, DEFAULT_ACTOR_TYPE, "default actor type");
        assert!(
            new.email.is_none() && new.password_hash.is_none(),
            "no contact details by default"
        );
        assert_eq!(new.name, "", "empty display name");
        assert_eq!(new.bio, "", "empty note");
        assert_eq!(new.raw_bio, "", "empty source note");
        assert_eq!(new.fields, json!([]), "empty field array");
        assert_eq!(new.raw_fields, json!([]), "empty raw field array");
        assert!(
            !new.locked.get() && !new.bot.get() && !new.discoverable.get(),
            "profile flags default to false"
        );
        assert!(
            !new.is_admin.get() && !new.is_moderator.get(),
            "roles default to false"
        );
        assert!(new.is_active.get(), "a new account is active");
        assert!(new.private_key.is_none(), "keys are opt-in");
        assert_eq!(
            (new.followers_count, new.following_count, new.statuses_count),
            (0, 0, 0),
            "counts default to zero"
        );
        assert!(
            new.hide_favorites.get()
                && new.allow_following_move.get()
                && new.accepts_chat_messages.get()
                && new.show_role.get()
                && new.email_confirmed.get()
                && new.email_notifications.get(),
            "the Pleroma flags whose column defaults to true are set"
        );
        assert!(
            !new.hide_followers.get()
                && !new.hide_follows.get()
                && !new.hide_followers_count.get()
                && !new.hide_follows_count.get()
                && !new.skip_thread_containment.get()
                && !new.no_rich_text.get()
                && !new.show_birthday.get()
                && !new.sensitive_default.get()
                && !new.approval_pending.get(),
            "and every other Pleroma flag is clear"
        );
        assert_eq!(
            new.default_scope, DEFAULT_SCOPE,
            "statuses default to the public scope"
        );
        assert!(
            new.tags.is_empty() && new.also_known_as.is_empty(),
            "the array columns start empty"
        );
        assert!(
            new.background_image_url.is_none()
                && new.birthday.is_none()
                && new.moved_to_ap_id.is_none()
                && new.last_active_at.is_none()
                && new.registration_reason.is_none()
                && new.language.is_none(),
            "the nullable columns start NULL"
        );
        assert_jsonb_defaults(&new);
    }

    /// The jsonb columns of a [`NewUser`], each at its column default:
    /// the settings blob empty, the MFA and notification blobs all-off.
    fn assert_jsonb_defaults(new: &NewUser) {
        assert_eq!(
            new.settings_store,
            json!({}),
            "the settings blob starts as an empty object"
        );
        assert_eq!(
            new.mfa_settings,
            mfa::MfaSettings::default_value(),
            "the MFA blob starts at the migration's all-off default"
        );
        assert_eq!(
            new.notification_settings,
            notification_settings::NotificationSettings::default_value(),
            "the notification flags start at the migration's all-off default"
        );
    }

    #[tokio::test]
    async fn set_also_known_as_replaces_the_array() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        let first = vec![local_ap_id(BASE, "alicia")];
        set_also_known_as(&pool, alice.id, &first).await.unwrap();
        let stored = fetch(&pool, alice.id).await;
        assert_eq!(
            stored.also_known_as, first,
            "the written array is what reads back"
        );

        set_also_known_as(&pool, alice.id, &[]).await.unwrap();
        assert!(
            fetch(&pool, alice.id).await.also_known_as.is_empty(),
            "an empty array clears what was there"
        );

        // An unknown id matches no row and is not an error.
        set_also_known_as(&pool, new_id(), &first).await.unwrap();
    }

    #[tokio::test]
    async fn mark_moved_records_target_and_stamp() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        assert!(alice.last_move_at.is_none(), "the fixture never moved");
        let target = "https://other.example/users/alice";
        mark_moved(&pool, alice.id, target).await.unwrap();
        let moved = fetch(&pool, alice.id).await;
        assert_eq!(
            moved.moved_to_ap_id.as_deref(),
            Some(target),
            "the target actor id is recorded"
        );
        assert!(
            moved.last_move_at.is_some(),
            "the cooldown stamp is set to the move time"
        );
    }

    #[tokio::test]
    async fn set_settings_store_replaces_the_blob() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        assert_eq!(alice.settings_store, json!({}), "the fixture starts empty");
        set_settings_store(&pool, alice.id, &json!({"pleroma_fe": {"theme": "dark"}}))
            .await
            .unwrap();
        let stored = fetch(&pool, alice.id).await;
        assert_eq!(
            stored.settings_store,
            json!({"pleroma_fe": {"theme": "dark"}}),
            "the written blob is what reads back"
        );

        set_settings_store(&pool, alice.id, &json!({}))
            .await
            .unwrap();
        assert_eq!(
            fetch(&pool, alice.id).await.settings_store,
            json!({}),
            "an empty object clears what was there"
        );

        // An unknown id matches no row and is not an error.
        set_settings_store(&pool, new_id(), &json!({"x": 1}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn set_preferred_frontend_stores_the_choice() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        assert!(alice.preferred_frontend.is_none(), "no choice yet");
        set_preferred_frontend(&pool, alice.id, "pleroma-fe")
            .await
            .unwrap();
        assert_eq!(
            fetch(&pool, alice.id).await.preferred_frontend.as_deref(),
            Some("pleroma-fe"),
            "the chosen frontend is what reads back"
        );

        set_preferred_frontend(&pool, alice.id, "mochi-pub")
            .await
            .unwrap();
        assert_eq!(
            fetch(&pool, alice.id).await.preferred_frontend.as_deref(),
            Some("mochi-pub"),
            "a new choice replaces the old one"
        );
    }

    #[tokio::test]
    async fn set_email_notifications_stores_the_flag() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        assert!(
            alice.email_notifications.get(),
            "an account starts opted in"
        );

        set_email_notifications(&pool, alice.id, false)
            .await
            .unwrap();
        assert!(
            !fetch(&pool, alice.id).await.email_notifications.get(),
            "the unsubscribe write is what reads back"
        );

        set_email_notifications(&pool, alice.id, true)
            .await
            .unwrap();
        assert!(
            fetch(&pool, alice.id).await.email_notifications.get(),
            "and opting back in flips it again"
        );

        // An unknown id matches no row and is not an error.
        set_email_notifications(&pool, new_id(), false)
            .await
            .unwrap();
    }

    /// A [`ProfileFields`] that sets every profile column to something
    /// distinct: images, notes, rendered and raw fields, the posting defaults,
    /// the actor type and every Pleroma preference.
    fn all_profile_fields() -> ProfileFields {
        ProfileFields {
            name: "Alice Adams".to_owned(),
            bio: "<p>hi</p>".to_owned(),
            raw_bio: "hi".to_owned(),
            locked: Flag::new(true),
            bot: Flag::new(false),
            discoverable: Flag::new(true),
            fields: json!([{"name": "site", "value": "<p><a href=\"https://alice.example\" rel=\"ugc nofollow noopener noreferrer\">alice.example</a></p>"}]),
            raw_fields: json!([{"name": "site", "value": "https://alice.example"}]),
            avatar_url: Some(format!("{BASE}/media/alice/avatar.png")),
            banner_url: Some(format!("{BASE}/media/alice/banner.png")),
            background_image_url: Some(format!("{BASE}/media/alice/bg.png")),
            default_scope: "private".to_owned(),
            sensitive_default: Flag::new(true),
            language: Some("pt-BR".to_owned()),
            pleroma: PleromaPrefs {
                hide_followers: Flag::new(true),
                hide_follows: Flag::new(true),
                hide_followers_count: Flag::new(true),
                hide_follows_count: Flag::new(true),
                hide_favorites: Flag::new(false),
                show_role: Flag::new(false),
                skip_thread_containment: Flag::new(true),
                allow_following_move: Flag::new(false),
                accepts_chat_messages: Flag::new(false),
                no_rich_text: Flag::new(true),
                show_birthday: Flag::new(true),
            },
            birthday: NaiveDate::from_ymd_opt(1990, 1, 31),
            settings_store: json!({"pleroma_fe": {"theme": "dark"}, "bookmarks": true}),
            actor_type: "Service".to_owned(),
        }
    }

    /// Every column [`update_profile`] was handed, on the row it returned —
    /// and the `updated_at` the write moved on.
    fn assert_profile_written(updated: &User, before: &User) {
        assert_eq!(updated.name, "Alice Adams", "the returned row is updated");
        assert_eq!(updated.bio, "<p>hi</p>", "the html note");
        assert_eq!(updated.raw_bio, "hi", "the source note");
        assert!(updated.locked.get(), "locked set");
        assert!(!updated.bot.get(), "bot left off");
        assert!(updated.discoverable.get(), "discoverable set");
        assert_eq!(
            updated.fields,
            json!([{"name": "site", "value": "<p><a href=\"https://alice.example\" rel=\"ugc nofollow noopener noreferrer\">alice.example</a></p>"}]),
            "the rendered fields are written"
        );
        assert_eq!(
            updated.raw_fields,
            json!([{"name": "site", "value": "https://alice.example"}]),
            "the raw fields are written beside them"
        );
        assert_eq!(
            updated.avatar_url.as_deref(),
            Some("https://test.example/media/alice/avatar.png"),
            "the avatar URL is written"
        );
        assert_eq!(
            updated.banner_url.as_deref(),
            Some("https://test.example/media/alice/banner.png"),
            "the banner URL is written"
        );
        assert_eq!(
            updated.background_image_url.as_deref(),
            Some("https://test.example/media/alice/bg.png"),
            "the background URL is written"
        );
        assert_eq!(updated.default_scope, "private", "the posting default");
        assert!(updated.sensitive_default.get(), "the sensitive default");
        assert_eq!(updated.language.as_deref(), Some("pt-BR"), "the language");
        assert_eq!(updated.actor_type, "Service", "the actor type");
        assert_eq!(
            updated.birthday,
            NaiveDate::from_ymd_opt(1990, 1, 31),
            "the birthday"
        );
        assert_eq!(
            updated.settings_store,
            json!({"pleroma_fe": {"theme": "dark"}, "bookmarks": true}),
            "the settings store is written as handed in"
        );
        assert!(
            updated.hide_followers.get()
                && updated.hide_follows.get()
                && updated.hide_followers_count.get()
                && updated.hide_follows_count.get()
                && updated.show_birthday.get()
                && updated.skip_thread_containment.get()
                && updated.no_rich_text.get(),
            "every set flag round trips"
        );
        assert!(
            !updated.hide_favorites.get()
                && !updated.show_role.get()
                && !updated.allow_following_move.get()
                && !updated.accepts_chat_messages.get(),
            "and so does every cleared one"
        );
        assert!(
            updated.updated_at > before.updated_at,
            "the write refreshes updated_at"
        );
    }

    #[tokio::test]
    async fn update_profile_writes_every_profile_column_and_returns_them() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = insert(&pool, &local_new("alice")).await.unwrap();
        assert_eq!(alice.name, "", "the fixture has no display name");
        assert!(
            !alice.locked.get() && !alice.bot.get() && !alice.discoverable.get(),
            "the option flags default off"
        );
        assert!(
            alice.avatar_url.is_none()
                && alice.banner_url.is_none()
                && alice.background_image_url.is_none(),
            "the fixture has no profile images"
        );
        let fields = all_profile_fields();
        let updated = update_profile(&pool, alice.id, &fields)
            .await
            .unwrap()
            .expect("the row exists");
        assert_profile_written(&updated, &alice);

        let stored = fetch(&pool, alice.id).await;
        assert_eq!(stored.name, updated.name, "the row is what reads back");
        assert_eq!(stored.bio, updated.bio, "stored bio");
        assert_eq!(stored.raw_bio, updated.raw_bio, "stored raw bio");
        assert_eq!(stored.fields, updated.fields, "stored fields");
        assert_eq!(stored.raw_fields, updated.raw_fields, "stored raw fields");
        assert_eq!(stored.avatar_url, updated.avatar_url, "stored avatar url");

        // An unknown id matches no row and is not an error.
        assert!(
            update_profile(&pool, new_id(), &fields)
                .await
                .unwrap()
                .is_none(),
            "no row, no update"
        );

        // The columns are plain nullable text: the merged value the caller
        // hands in is what stands, so `None` clears one image while the
        // other two keep what the caller merged over the row.
        let cleared = ProfileFields {
            avatar_url: None,
            ..fields
        };
        let after = update_profile(&pool, alice.id, &cleared)
            .await
            .unwrap()
            .expect("the row exists");
        assert!(after.avatar_url.is_none(), "the avatar column is cleared");
        assert_eq!(
            after.banner_url, stored.banner_url,
            "and the untouched ones keep their value"
        );
        assert_eq!(
            after.background_image_url, stored.background_image_url,
            "the background keeps its value"
        );
    }

    /// A discoverable local user named `nickname` whose newest status was
    /// posted at `active` (`None`: never).
    async fn discoverable(pool: &PgPool, nickname: &str, active: Option<DateTime<Utc>>) -> User {
        insert(
            pool,
            &NewUser {
                discoverable: Flag::new(true),
                last_status_at: active,
                ..local_new(nickname)
            },
        )
        .await
        .expect("the discoverable fixture inserts")
    }

    /// Moves `target`'s `inserted_at` back to `days` ago, so an
    /// `order = new` expectation does not depend on how fast the fixture
    /// loop ran.
    async fn insert_days_ago(pool: &PgPool, target: &User, days: i64) {
        let moved = sqlx::query(
            "UPDATE users SET inserted_at = now() - $1::bigint * interval '1 day' WHERE id = $2",
        )
        .bind(days)
        .bind(target.id)
        .execute(pool)
        .await
        .expect("the fixture backdates the account");
        assert_eq!(moved.rows_affected(), 1, "the row was there to move");
    }

    /// The ids of `users`, in the order the query answered them.
    fn ids_of(users: &[User]) -> Vec<Uuid> {
        users.iter().map(|found| found.id).collect()
    }

    /// The directory page `query` asks for, with this endpoint's page sizes.
    fn directory_page_params(query: &str) -> PageParams {
        PageParams::from_query_with_default(Some(query), 40, 80).expect("the fixture page parses")
    }

    #[tokio::test]
    async fn directory_page_needs_discoverable_and_active() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let listed = discoverable(&pool, "listed", None).await;
        let hidden = insert(&pool, &local_new("hidden")).await.unwrap();
        let gone = discoverable(&pool, "gone", None).await;
        let _deactivated = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(gone.id)
            .execute(&pool)
            .await
            .expect("the fixture deactivates");

        let seen = directory_page(
            &pool,
            DirectoryOrder::Active,
            false,
            directory_page_params(""),
        )
        .await
        .unwrap();
        assert_eq!(ids_of(&seen), vec![listed.id], "opted in and active");
        assert!(
            !ids_of(&seen).contains(&hidden.id),
            "an account that never opted into discovery is not listed"
        );
        assert!(
            !ids_of(&seen).contains(&gone.id),
            "and neither is a deactivated one"
        );
    }

    #[tokio::test]
    async fn directory_page_lists_remote_accounts_unless_asked_locally() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let local = discoverable(&pool, "local-one", None).await;
        let ap_id = "https://remote.example/users/mirror";
        let mirror = insert(
            &pool,
            &NewUser {
                nickname: "mirror@remote.example".to_owned(),
                ap_id: ap_id.to_owned(),
                domain: Some("remote.example".to_owned()),
                discoverable: Flag::new(true),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote fixture inserts");

        let everyone = directory_page(
            &pool,
            DirectoryOrder::Active,
            false,
            directory_page_params(""),
        )
        .await
        .unwrap();
        assert_eq!(
            ids_of(&everyone).len(),
            2,
            "with no `local` flag the mirrors stay in, as Pleroma's \
             `exclude_remote` only fires on true"
        );
        let local_only = directory_page(
            &pool,
            DirectoryOrder::Active,
            true,
            directory_page_params(""),
        )
        .await
        .unwrap();
        assert_eq!(
            ids_of(&local_only),
            vec![local.id],
            "`local` true answers this instance's own accounts"
        );
        assert!(!ids_of(&local_only).contains(&mirror.id), "and no mirror");
    }

    #[tokio::test]
    async fn directory_active_order_puts_never_posted_accounts_last() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let quiet = discoverable(&pool, "quiet", None).await;
        let older =
            discoverable(&pool, "older", Some(Utc::now() - chrono::Duration::days(1))).await;
        let newest = discoverable(&pool, "newest", Some(Utc::now())).await;

        let seen = directory_page(
            &pool,
            DirectoryOrder::Active,
            false,
            directory_page_params(""),
        )
        .await
        .unwrap();
        assert_eq!(
            ids_of(&seen),
            vec![newest.id, older.id, quiet.id],
            "newest status first, the accounts that never posted last"
        );
    }

    #[tokio::test]
    async fn directory_new_order_sorts_on_the_account_itself() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        // The newest account posted long ago: the two orders must disagree.
        let fresh =
            discoverable(&pool, "fresh", Some(Utc::now() - chrono::Duration::days(9))).await;
        let veteran = discoverable(
            &pool,
            "veteran",
            Some(Utc::now() - chrono::Duration::days(1)),
        )
        .await;
        insert_days_ago(&pool, &veteran, 5).await;
        insert_days_ago(&pool, &fresh, 1).await;

        let seen = directory_page(&pool, DirectoryOrder::New, false, directory_page_params(""))
            .await
            .unwrap();
        assert_eq!(
            ids_of(&seen),
            vec![fresh.id, veteran.id],
            "`new` sorts on `inserted_at`, ignoring last activity"
        );
        let by_activity = directory_page(
            &pool,
            DirectoryOrder::Active,
            false,
            directory_page_params(""),
        )
        .await
        .unwrap();
        assert_eq!(
            ids_of(&by_activity),
            vec![veteran.id, fresh.id],
            "and the default order is the other way round for the same rows"
        );
    }

    #[tokio::test]
    async fn directory_pages_by_limit_and_offset() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        for nickname in ["one", "two", "three", "four", "five"] {
            let posted = discoverable(&pool, nickname, Some(Utc::now())).await;
            assert!(posted.discoverable.get(), "{nickname} is listed");
        }
        let all = directory_page(&pool, DirectoryOrder::New, false, directory_page_params(""))
            .await
            .unwrap();
        assert_eq!(ids_of(&all).len(), 5, "the whole listing");

        let second = directory_page(
            &pool,
            DirectoryOrder::New,
            false,
            directory_page_params("limit=2&offset=2"),
        )
        .await
        .unwrap();
        assert_eq!(
            ids_of(&second),
            ids_of(&all)[2..4],
            "offset 2 skips the first two of the same order"
        );
        let tail = directory_page(
            &pool,
            DirectoryOrder::New,
            false,
            directory_page_params("limit=2&offset=4"),
        )
        .await
        .unwrap();
        assert_eq!(tail.len(), 1, "and the last page is what is left");
        assert_eq!(ids_of(&tail), ids_of(&all)[4..], "the fifth account");
        let past = directory_page(
            &pool,
            DirectoryOrder::New,
            false,
            directory_page_params("offset=99"),
        )
        .await
        .unwrap();
        assert!(past.is_empty(), "an offset past the end answers nothing");
    }

    /// A local user named `nickname` born on `birthday`, showing it only
    /// when `show` says so.
    async fn born(pool: &PgPool, nickname: &str, birthday: Option<NaiveDate>, show: bool) -> User {
        insert(
            pool,
            &NewUser {
                birthday,
                show_birthday: Flag::new(show),
                ..local_new(nickname)
            },
        )
        .await
        .expect("the birthday fixture inserts")
    }

    /// `year-month-day` as a date, from the parts a test names.
    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).expect("the fixture date is a real day")
    }

    /// Deactivates `target` — the state a suspended or closed account is in.
    async fn deactivate(pool: &PgPool, target: &User) {
        let gone = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
            .bind(target.id)
            .execute(pool)
            .await
            .expect("the fixture deactivates");
        assert_eq!(gone.rows_affected(), 1, "the row was there to close");
    }

    /// Records `follower`'s accepted follow of the local `target`.
    async fn follow(pool: &PgPool, follower: &User, target: &User) {
        let row = crate::models::follow::follow(pool, BASE, follower, target)
            .await
            .expect("the fixture follow records");
        assert_eq!(
            row.state,
            crate::models::follow::FollowState::Accepted,
            "an open local account answers on the spot"
        );
    }

    #[tokio::test]
    async fn birthday_accounts_answers_the_viewer_and_their_follows() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let day = date(1990, 5, 17);
        let alice = born(&pool, "alice", Some(day), true).await;
        let followed = born(&pool, "followed", Some(day), true).await;
        let unfollowed = born(&pool, "unfollowed", Some(day), true).await;
        let private = born(&pool, "private", Some(day), false).await;
        let closed = born(&pool, "closed", Some(day), true).await;
        let blank = born(&pool, "blank", None, true).await;
        let wrong_day = born(&pool, "wrong-day", Some(date(1990, 5, 18)), true).await;
        deactivate(&pool, &closed).await;
        // A remote follow counts once it is accepted: the request a mirror
        // leaves pending is no follow for the listing.
        let ap_id = "https://remote.example/users/mirror";
        let mirror = insert(
            &pool,
            &NewUser {
                nickname: "mirror@remote.example".to_owned(),
                ap_id: ap_id.to_owned(),
                domain: Some("remote.example".to_owned()),
                birthday: Some(day),
                show_birthday: Flag::new(true),
                ..NewUser::default()
            },
        )
        .await
        .expect("the remote birthday fixture inserts");
        follow(&pool, &alice, &followed).await;

        let seen = ids_of(&birthday_accounts(&pool, alice.id, 5, 17).await.unwrap());
        assert!(
            seen.contains(&alice.id),
            "the viewer is in their own birthday list"
        );
        assert!(seen.contains(&followed.id), "so is an accepted follow");
        for absent in [unfollowed.id, private.id, closed.id, blank.id, wrong_day.id] {
            assert!(
                !seen.contains(&absent),
                "{absent} is not on the list: an account the viewer does not \
                 follow, one that hides its birthday, a closed one, one with \
                 no birthday and one born on another day are all out"
            );
        }

        let request = crate::models::follow::follow(&pool, BASE, &alice, &mirror)
            .await
            .expect("the outgoing request records");
        assert_eq!(
            request.state,
            crate::models::follow::FollowState::Pending,
            "a mirror answers nothing on the spot"
        );
        assert_eq!(
            ids_of(&birthday_accounts(&pool, alice.id, 5, 17).await.unwrap()),
            seen,
            "a pending request to a remote account adds nobody"
        );
        let accepted = crate::models::follow::accept(&pool, request.id)
            .await
            .expect("the fixture's other side accepts it");
        assert_eq!(
            accepted.state,
            crate::models::follow::FollowState::Accepted,
            "the request is answered"
        );
        let after = ids_of(&birthday_accounts(&pool, alice.id, 5, 17).await.unwrap());
        assert!(
            after.contains(&mirror.id),
            "and the accepted follow of a remote account is in: local and \
             remote follows count alike"
        );
    }

    #[tokio::test]
    async fn a_leap_day_birthday_matches_the_leap_day_only() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let alice = born(&pool, "alice", Some(date(2000, 2, 29)), true).await;

        let on = ids_of(&birthday_accounts(&pool, alice.id, 2, 29).await.unwrap());
        assert_eq!(on, vec![alice.id], "2/29 finds the account born on it");
        for (month, day) in [(2, 28), (3, 29), (2, 30), (13, 29)] {
            assert!(
                birthday_accounts(&pool, alice.id, month, day)
                    .await
                    .unwrap()
                    .is_empty(),
                "and {month}/{day} does not: the match is on both calendar \
                 parts, so in a year without a 29th of February the list is \
                 simply empty"
            );
        }
    }

    #[tokio::test]
    async fn birthday_accounts_orders_newest_account_first() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let day = date(1990, 12, 25);
        let alice = born(&pool, "alice", Some(day), true).await;
        let first = born(&pool, "first", Some(day), true).await;
        let second = born(&pool, "second", Some(day), true).await;
        follow(&pool, &alice, &first).await;
        follow(&pool, &alice, &second).await;

        let mut expected = vec![alice.id, first.id, second.id];
        // `ORDER BY id DESC` on `UUIDv7` text is creation order (decision 4);
        // the fixture ids are minted in the same millisecond, so the
        // expectation is the byte order the database sorts on, not the
        // insert order.
        expected.sort_by(|left, right| right.cmp(left));
        let seen = ids_of(&birthday_accounts(&pool, alice.id, 12, 25).await.unwrap());
        assert_eq!(
            seen, expected,
            "newest account first, the viewer among them"
        );
        assert!(
            birthday_accounts(&pool, alice.id, 1, 1)
                .await
                .unwrap()
                .is_empty(),
            "a day nobody the viewer knows is born on answers nothing, \
             which is no error"
        );
    }
}
