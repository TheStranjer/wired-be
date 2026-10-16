//! The Mastodon `Mention` entity: one @-mentioned account, as the Status
//! entity's `mentions` array renders it.
//!
//! The reference shape is Pleroma's `AccountView.render("mention.json", …)`
//! (`lib/pleroma/web/mastodon_api/views/account_view.ex`): the four keys
//! `id`, `username`, `url` and `acct`, and nothing else. Like the
//! [`Account`](crate::entities::account::Account) render it is pure — a
//! [`User`] row becomes the entity without a database or a request, from
//! the stored handle and URLs alone.

use serde::Serialize;

use crate::{entities::account::nickname_part, models::user::User};

/// The `mentions` entry of a Status entity — the account one post
/// @-mentioned, in the four-key shape Mastodon clients read.
#[derive(Debug, Serialize)]
pub struct Mention {
    /// The mentioned account's id: its `UUIDv7` as the hyphenated lowercase
    /// string (decision 4).
    pub id: String,
    /// The local part of the handle: `nick`, without any `@domain` suffix.
    pub username: String,
    /// The account's profile URL: the stored [`profile_url`](User::profile_url)
    /// of a remote mirror, or the actor id every row is guaranteed to carry
    /// — the same fallback the `Account` entity renders its `url` with.
    pub url: String,
    /// The full account name: the bare `nick` for a local user and
    /// `nick@domain` for a remote one, exactly as
    /// [`User::nickname`] stores it (decision 7).
    pub acct: String,
}

impl Mention {
    /// Renders `user` as the mention entity: Pleroma's `mention.json`, with
    /// `url` falling back from the profile URL to the actor id and `acct`
    /// repeating the stored handle.
    #[must_use]
    pub fn from_user(user: &User) -> Self {
        Self {
            id: user.id.to_string(),
            username: nickname_part(&user.nickname).to_owned(),
            url: user
                .profile_url
                .clone()
                .unwrap_or_else(|| user.ap_id.clone()),
            acct: user.nickname.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone as _, Utc};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::TEST_BASE_URL,
        models::user::{DEFAULT_SCOPE, Flag, mfa},
    };

    /// The fixture account's id, as stored.
    const ACCOUNT_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";

    /// A `User` at the column defaults a fresh account carries, under
    /// `nickname` and `ap_id`.
    fn base_user(nickname: &str, ap_id: &str) -> User {
        User {
            id: Uuid::parse_str(ACCOUNT_ID).unwrap(),
            nickname: nickname.to_owned(),
            ap_id: ap_id.to_owned(),
            local: Flag::new(false),
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
            mfa_settings: mfa::MfaSettings::default_value(),
            background_image_url: None,
            birthday: None,
            show_birthday: Flag::new(false),
            also_known_as: Vec::new(),
            moved_to_ap_id: None,
            last_move_at: None,
            email_confirmed: Flag::new(true),
            approval_pending: Flag::new(false),
            registration_reason: None,
            last_active_at: None,
            followers_count: 0,
            following_count: 0,
            statuses_count: 0,
            last_status_at: None,
            last_fetched_at: None,
            inserted_at: Utc.with_ymd_and_hms(2024, 1, 31, 12, 34, 56).unwrap(),
            updated_at: Utc.with_ymd_and_hms(2024, 1, 31, 12, 34, 56).unwrap(),
            preferred_frontend: None,
            is_suggested: Flag::new(false),
        }
    }

    /// The rendered JSON key set, for the shape assertions.
    fn rendered(user: &User) -> Value {
        serde_json::to_value(Mention::from_user(user)).unwrap()
    }

    #[test]
    fn a_local_mention_carries_the_bare_nickname_and_actor_urls() {
        let mut alice = base_user("alice", &format!("{TEST_BASE_URL}/users/alice"));
        alice.local = Flag::new(true);
        let value = rendered(&alice);
        assert_eq!(
            value,
            json!({
                "id": alice.id.to_string(),
                "username": "alice",
                "url": format!("{TEST_BASE_URL}/users/alice"),
                "acct": "alice",
            }),
            "the four keys, the acct bare, the id the hyphenated string"
        );
    }

    #[test]
    fn a_local_mention_with_a_profile_url_renders_that_url() {
        let mut alice = base_user("alice", &format!("{TEST_BASE_URL}/users/alice"));
        alice.local = Flag::new(true);
        alice.profile_url = Some(format!("{TEST_BASE_URL}/@alice"));
        assert_eq!(
            rendered(&alice)["url"],
            format!("{TEST_BASE_URL}/@alice"),
            "the stored profile URL wins over the actor id"
        );
    }

    #[test]
    fn a_remote_mention_splits_username_from_acct() {
        let bob = base_user("bob@remote.example", "https://remote.example/users/bob");
        assert_eq!(
            rendered(&bob),
            json!({
                "id": bob.id.to_string(),
                "username": "bob",
                "url": "https://remote.example/users/bob",
                "acct": "bob@remote.example",
            }),
            "the acct keeps the domain, the username drops it, and the url \
             falls back to the remote actor id"
        );
    }

    #[test]
    fn a_remote_mention_with_a_profile_url_renders_that_url() {
        let mut bob = base_user("bob@remote.example", "https://remote.example/users/bob");
        bob.profile_url = Some("https://remote.example/@bob".to_owned());
        assert_eq!(
            rendered(&bob)["url"],
            "https://remote.example/@bob",
            "a mirror fetched with a human URL renders it, as Account does"
        );
    }
}
