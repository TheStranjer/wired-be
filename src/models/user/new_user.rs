//! The [`NewUser`] shape: every column [`insert`](super::insert) accepts
//! from the caller, its redacting `Debug` impl, the [`Default`] impl that
//! mirrors the column defaults, and the [`NewUser::local`] constructor.

use std::fmt;

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{Value, json};

use super::{DEFAULT_ACTOR_TYPE, DEFAULT_SCOPE, Flag, mfa, notification_settings};

/// What to insert: every `users` column except the ones [`insert`](super::insert) mints
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
    /// Multi-factor authentication state; the all-off [`mfa::MfaSettings`]
    /// blob by default, matching the column default. Holds the TOTP
    /// secret; never in `Debug` output.
    pub mfa_settings: Value,
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

/// [`password_hash`](Self::password_hash) is a secret and renders as
/// `<redacted>`; [`mfa_settings`](Self::mfa_settings) holds the TOTP secret and
/// is redacted too; everything else shows.
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
            .field("mfa_settings", &"<redacted>")
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
            mfa_settings: mfa::MfaSettings::default_value(),
            is_suggested: Flag::new(false),
            password_reset_pending: Flag::new(false),
            email_notifications: Flag::new(true),
        }
    }
}

impl NewUser {
    /// A new local account named `nickname` with actor id `ap_id`: the
    /// column defaults of [`NewUser::default`] plus `local = true`. Pair
    /// it with [`local_ap_id`](super::local_ap_id) to build `ap_id`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::user::test_support::*;

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
}
