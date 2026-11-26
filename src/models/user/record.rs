//! The [`User`] row: one record of `users` (decision 7), the `Debug` impl
//! that keeps its secrets out of logs, and the [`User::account_status`]
//! read that weighs the account-state flags.

use std::fmt;

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::Value;
use uuid::Uuid;

use super::{AccountStatus, Flag};
use crate::util::password::verify_password;

/// One row of `users`: a local account or a mirror of a remote actor
/// (decision 7).
#[derive(Clone, sqlx::FromRow)]
pub struct User {
    /// `UUIDv7` primary key, minted by [`insert`](super::insert) (decision 4).
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
    /// [`notification_settings::NotificationSettings`](super::notification_settings::NotificationSettings)
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
    /// Number of followers; moved by [`update_counts`](super::update_counts).
    pub followers_count: i32,
    /// Number of follows; moved by [`update_counts`](super::update_counts).
    pub following_count: i32,
    /// Number of posted statuses; moved by [`update_counts`](super::update_counts).
    pub statuses_count: i32,
    /// Insert time of the newest status this user posted.
    pub last_status_at: Option<DateTime<Utc>>,
    /// When the row was inserted.
    pub inserted_at: DateTime<Utc>,
    /// When the row last changed; refreshed by every update here.
    pub updated_at: DateTime<Utc>,
    /// Multi-factor authentication state: the jsonb blob parsed by
    /// [`mfa::MfaSettings`](super::mfa::MfaSettings) (enabled flag, TOTP secret and confirmation,
    /// backup-code hashes). Holds the TOTP secret; never in `Debug`
    /// output.
    pub mfa_settings: Value,
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
    /// /api/pleroma/delete_account`, see [`mark_deleted`](super::mark_deleted)); `None` while
    /// it lives. The row is kept for referential integrity even after the
    /// purge; this is what tells a deleted account from a merely
    /// deactivated one.
    pub deleted_at: Option<DateTime<Utc>>,
    /// When a local suspension deactivated the account; `None` for an
    /// account that is active or only disabled (WBE-878).
    pub suspended_at: Option<DateTime<Utc>>,
    /// Whether the account's origin server marks its actor `suspended`
    /// (WBE-873); only the remote actor upsert writes it.
    pub remote_suspended: Flag,
}

/// [`password_hash`](Self::password_hash) and [`mfa_settings`](Self::mfa_settings)
/// are secrets and render as `<redacted>`; everything else shows.
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
            .field("inserted_at", &self.inserted_at)
            .field("updated_at", &self.updated_at)
            .field("mfa_settings", &"<redacted>")
            .field("is_suggested", &self.is_suggested)
            .field("password_reset_pending", &self.password_reset_pending)
            .field("email_notifications", &self.email_notifications)
            .field("deleted_at", &self.deleted_at)
            .field("suspended_at", &self.suspended_at)
            .field("remote_suspended", &self.remote_suspended)
            .finish()
    }
}

impl User {
    /// Whether this is one of this instance's own accounts and is not
    /// deactivated: the test every public, federated or account-bound
    /// surface applies before it serves or acts for a user.
    #[must_use]
    pub const fn is_local_active(&self) -> bool {
        self.local.get() && self.is_active.get()
    }

    /// Whether this account stands suspended rather than merely disabled:
    /// a local suspension ([`suspended_at`](Self::suspended_at)), its origin
    /// server's suspension ([`remote_suspended`](Self::remote_suspended)) or a
    /// deletion under way ([`deleted_at`](Self::deleted_at)).
    ///
    /// The one reading behind the `suspended` of the account entities and of
    /// the federated actor document; `disabled` is
    /// [`is_active`](Self::is_active) off without this.
    #[must_use]
    pub const fn is_suspended(&self) -> bool {
        self.suspended_at.is_some() || self.remote_suspended.get() || self.deleted_at.is_some()
    }

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

    /// Whether `password` matches the stored hash. A row without a hash (a
    /// remote mirror, a bot-created account) fails exactly like a wrong
    /// password, as Pleroma's `confirm_current_password` does.
    ///
    /// The MFA and account-settings endpoints both confirm the current
    /// password through this one check.
    #[must_use]
    pub fn password_matches(&self, password: &str) -> bool {
        self.password_hash
            .as_deref()
            .is_some_and(|hash| verify_password(password, hash))
    }

    /// The URL a profile links to: the stored
    /// [`profile_url`](User::profile_url) when set (what Pleroma's
    /// `Formatter.mention_from_user` prefers as `user.uri`), else the actor
    /// id every row is guaranteed to carry.
    #[must_use]
    pub fn profile_url_or_ap_id(&self) -> &str {
        self.profile_url.as_deref().unwrap_or(&self.ap_id)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::models::user::{NewUser, get_by_nickname, insert, test_support::*};

    #[tokio::test]
    async fn password_matches_accepts_only_the_stored_password() {
        const PASSWORD: &str = "correct horse battery staple";
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let hash = crate::util::password::hash_password(PASSWORD).expect("hashing works");
        let mut user = insert(
            &pool,
            &NewUser {
                password_hash: Some(hash),
                ..local_new("frank")
            },
        )
        .await
        .unwrap();
        assert!(user.password_matches(PASSWORD), "the right password");
        assert!(!user.password_matches("wrong password"), "a wrong password");
        assert!(!user.password_matches(""), "an empty password");

        user.password_hash = None;
        assert!(
            !user.password_matches(PASSWORD),
            "a hashless row fails like a wrong password"
        );
    }

    #[tokio::test]
    async fn profile_url_or_ap_id_prefers_the_stored_profile_url() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let bare = insert(&pool, &local_new("plain")).await.unwrap();
        assert_eq!(
            bare.profile_url_or_ap_id(),
            bare.ap_id,
            "without a profile_url the actor id stands in"
        );
        let profile = format!("{BASE}/@fancy");
        let with_url = insert(
            &pool,
            &NewUser {
                profile_url: Some(profile.clone()),
                ..local_new("fancy")
            },
        )
        .await
        .unwrap();
        assert_eq!(
            with_url.profile_url_or_ap_id(),
            profile,
            "a stored profile_url wins over the actor id"
        );
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
            discoverable: Flag::new(true),
            followers_count: 12,
            following_count: 3,
            statuses_count: 48,
            ..NewUser::default()
        };
        let user = insert(&pool, &new).await.unwrap();
        assert!(!user.local.get(), "the default NewUser is a remote mirror");
        assert_eq!(
            user.nickname, "bob@remote.example",
            "the user@host handle round trips"
        );
        assert_eq!(user.domain.as_deref(), Some("remote.example"), "domain");
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
    async fn is_local_active_needs_both_flags() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let mut row = insert(&pool, &local_new("flags")).await.unwrap();
        for (local, active, expected) in [
            (true, true, true),
            (true, false, false),
            (false, true, false),
            (false, false, false),
        ] {
            row.local = Flag::new(local);
            row.is_active = Flag::new(active);
            assert_eq!(
                row.is_local_active(),
                expected,
                "local={local}, active={active}"
            );
        }
    }

    #[tokio::test]
    async fn debug_redacts_secrets_in_user_and_new_user() {
        let db = crate::test_db::test_db().await.expect("test database slot");
        let pool = db.pool().clone();
        let user = insert(
            &pool,
            &NewUser {
                password_hash: Some(SECRET_HASH.to_owned()),
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
                && !new_debug.contains(SECRET_TOTP)
                && !new_debug.contains(SECRET_CONFIRMATION),
            "NewUser redacts the same secrets"
        );
        assert!(
            new_debug.contains(r#"password_hash: "<redacted>""#),
            "and shows the markers"
        );
    }
}
