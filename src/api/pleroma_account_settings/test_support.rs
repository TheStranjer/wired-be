//! Fixtures the account settings unit tests share: an alias list and a bare
//! user row.

use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

use crate::models::user::{self, User};

/// An account id the fixtures hand to the pure helpers.
pub(super) const ALIAS: &str = "https://old.example/users/alice";

/// A [`User`]-shaped alias list for the pure list-editing assertions.
pub(super) fn aliases(extra: &[&str]) -> Vec<String> {
    let mut list = vec![ALIAS.to_owned()];
    list.extend(extra.iter().map(|alias| (*alias).to_owned()));
    list
}
/// A bare [`User`] with the two fields the pure helpers read and the
/// rest at the constructor defaults (the shape of a fresh row).
pub(super) fn template_user(nickname: &str) -> User {
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
        inserted_at: Utc::now(),
        updated_at: Utc::now(),
        mfa_settings: user::mfa::MfaSettings::default_value(),
        is_suggested: user::Flag::new(false),
        password_reset_pending: user::Flag::new(false),
        email_notifications: user::Flag::new(true),
        deleted_at: None,
        suspended_at: None,
        remote_suspended: user::Flag::new(false),
    }
}
