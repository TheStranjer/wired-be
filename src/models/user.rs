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
//! letter and its two writers live in [`confirmation`]; and the admin API's
//! filtered listing and nickname-or-id lookup live in [`admin`].
//!
//! The rest of the table's work is split by job behind this file: the
//! [`User`] record, the [`NewUser`] shape and its [`insert`], the single
//! reads and the MRF preload reads, the directory and birthday listings,
//! the whole-column setters, the credential and deletion writes, and the
//! profile write. Each lives in its own submodule and is re-exported
//! here, so every outside path stays `crate::models::user::…`.

mod birthdays;
mod credentials;
mod deletion;
mod directory;
mod inboxes;
mod insert;
mod lookup;
mod mrf_preload;
mod new_user;
mod profile;
mod record;
mod setters;

pub mod admin;
pub mod confirmation;
pub mod mfa;
pub mod notification_settings;
pub mod register;

#[cfg(test)]
mod test_support;

pub use birthdays::birthday_accounts;
pub use credentials::{set_activation, set_email, set_password, set_password_reset_pending};
pub use deletion::{mark_deleted, mark_deleted_by_domain, purge_personal_data};
pub use directory::{DirectoryOrder, directory_page};
pub use inboxes::all_remote_inboxes;
pub use insert::{CountDelta, insert};
pub use lookup::{
    active_local_ids, find_password_reset_target, get_by_acct, get_by_ap_id, get_by_email,
    get_by_id, get_by_nickname, get_by_public_key_id, get_many, require_local_active,
};
pub use mrf_preload::{counts_by_ap_id, local_tags_by_ap_id, tags_by_ap_id};
pub use new_user::NewUser;
pub use profile::{PleromaPrefs, ProfileFields, update_profile};
pub use record::User;
pub use setters::{
    mark_moved, set_also_known_as, set_email_notifications, set_moved_to, set_nickname,
    set_settings_store, update_counts,
};

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

/// [`NOT_INSTANCE_ACTOR`] for a query that aliases `users` as `u`:
/// ` AND u.nickname <> 'internal.fetch'`.
///
/// The literal is kept in step with [`INSTANCE_ACTOR_NICKNAME`] by a test.
pub const NOT_INSTANCE_ACTOR_U: &str = " AND u.nickname <> 'internal.fetch'";

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

/// The address naming `user`'s followers collection.
///
/// It is the stored `followers_url` (what the publisher expands, and what a
/// peer's own documents name), with the [`followers`] construction behind a
/// row that carries none.
#[must_use]
pub fn followers_address(user: &User) -> String {
    user.followers_url
        .clone()
        .unwrap_or_else(|| followers(&user.ap_id))
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
    use super::test_support::BASE;

    use super::*;

    /// The shared exclusion fragment must name exactly the instance actor
    /// nickname, or the listing queries and the service would drift apart.
    #[test]
    fn the_instance_actor_exclusion_names_the_instance_actor() {
        assert!(
            NOT_INSTANCE_ACTOR.contains(INSTANCE_ACTOR_NICKNAME),
            "the SQL fragment excludes the account the constant names"
        );
        assert!(
            NOT_INSTANCE_ACTOR_U.contains(INSTANCE_ACTOR_NICKNAME),
            "so does the `u`-aliased fragment"
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
}
