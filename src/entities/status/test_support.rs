//! The unit-test fixtures the `status` children share: the stored rows a
//! render is measured against — a live local public status by a local
//! author, and second accounts to act as another author, a viewer or a
//! reactor — the instant every fixture is stamped with, the key sets a
//! response must match exactly, and the small row builders (attachment,
//! hashtag, reaction roll-up) with the JSON helper the assertions compare.
//!
//! The batch a test render hands the entity is the `parts` child:
//! [`Parts`] with its one-line setters, [`render`] to get the entity as
//! JSON and [`render_nested`] to get it as the nested entity a reblog
//! embeds.

use chrono::{DateTime, Utc};
use serde_json::json;
use uuid::Uuid;

use crate::{
    config::TEST_BASE_URL,
    models::{
        custom_emoji::CustomEmoji as EmojiRow,
        hashtag::Hashtag,
        media_attachment::MediaAttachment as AttachmentRow,
        status::{Status as StatusRow, Visibility},
        user::{Flag, User, mfa},
    },
    util::time::parse_time,
};

mod parts;

// The rendered object's keys, sorted, for exact key-set assertions.
pub(super) use crate::test_fixtures::json_keys;

pub(super) use self::parts::{Parts, render, render_nested};

/// The instance origin [`Config::for_tests`] serves.
pub(super) const BASE: &str = TEST_BASE_URL;
/// The author's id, as stored.
pub(super) const AUTHOR_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
/// A second account id, for the reactor lists.
pub(super) const REACTOR_ID: &str = "01934567-89ab-7cde-f012-3456789abcee";
/// A custom emoji's image URL, for the reaction entries.
pub(super) const CAT_IMAGE: &str = "https://test.example/emoji/cats/cat.png";
/// The URL a link card is cached under, for the card entries.
pub(super) const CARD_URL: &str = "https://example.test/post";
/// Every JSON key the entity must emit, sorted.
pub(super) const EXPECTED_KEYS: [&str; 34] = [
    "account",
    "application",
    "bookmarked",
    "card",
    "content",
    "created_at",
    "edited_at",
    "emojis",
    "favourited",
    "favourites_count",
    "filtered",
    "id",
    "in_reply_to_account_id",
    "in_reply_to_id",
    "language",
    "media_attachments",
    "mentions",
    "muted",
    "pinned",
    "pleroma",
    "poll",
    "quote",
    "quote_approval",
    "quote_id",
    "reblog",
    "reblogged",
    "reblogs_count",
    "replies_count",
    "sensitive",
    "spoiler_text",
    "tags",
    "uri",
    "url",
    "visibility",
];
/// Every `pleroma` block key, sorted.
pub(super) const PLEROMA_KEYS: [&str; 17] = [
    "bookmark_folder",
    "content",
    "conversation_id",
    "direct_conversation_id",
    "emoji_reactions",
    "expires_at",
    "in_reply_to_account_acct",
    "local",
    "parent_visible",
    "pinned_at",
    "quote",
    "quote_id",
    "quote_url",
    "quote_visible",
    "quotes_count",
    "spoiler_text",
    "thread_muted",
];

/// The instant every fixture row is stamped with, in and out.
pub(super) fn stamped() -> DateTime<Utc> {
    parse_time("2024-01-31T12:34:56.789Z").unwrap()
}

/// A [`User`] row at the column defaults a fresh local account
/// carries, named `nickname`.
pub(super) fn base_user(nickname: &str) -> User {
    User {
        id: Uuid::parse_str(AUTHOR_ID).unwrap(),
        nickname: nickname.to_owned(),
        ap_id: format!("{BASE}/users/{nickname}"),
        local: Flag::new(true),
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
        default_scope: "public".to_owned(),
        sensitive_default: Flag::new(false),
        language: None,
        tags: Vec::new(),
        settings_store: json!({}),
        notification_settings: json!({}),
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
        confirmation_token: None,
        confirmation_sent_at: None,
        last_active_at: None,
        followers_count: 0,
        following_count: 0,
        statuses_count: 0,
        last_status_at: None,
        inserted_at: stamped(),
        updated_at: stamped(),
        is_suggested: Flag::new(false),
        password_reset_pending: Flag::new(false),
        email_notifications: Flag::new(true),
        deleted_at: None,
        suspended_at: None,
        disabled_at: None,
        remote_suspended: Flag::new(false),
    }
}

/// A second user, distinct from [`base_user`]'s author row: `nickname`
/// with id `id`.
pub(super) fn other_user(nickname: &str, id: &str) -> User {
    let mut user = base_user(nickname);
    user.id = Uuid::parse_str(id).unwrap();
    user
}

/// A live local public status by [`base_user`]'s author: every link
/// empty, every counter zero, the timestamps fixed.
pub(super) fn base_row() -> StatusRow {
    let id = Uuid::parse_str("01944444-4444-7444-8444-444444444444").unwrap();
    StatusRow {
        id,
        uri: format!("{BASE}/objects/{id}"),
        url: Some(format!("{BASE}/notice/{id}")),
        user_id: Uuid::parse_str(AUTHOR_ID).unwrap(),
        content: "<p>hello world</p>".to_owned(),
        raw_text: "hello world".to_owned(),
        content_type: "text/plain".to_owned(),
        spoiler_text: String::new(),
        sensitive: false,
        visibility: Visibility::Public,
        language: None,
        in_reply_to_id: None,
        in_reply_to_account_id: None,
        in_reply_to_ap_id: None,
        conversation_id: id,
        ap_context: None,
        direct_conversation_id: None,
        reblog_of_id: None,
        quote_of_id: None,
        quote_policy: None,
        quote_approval_policy: None,
        local: true,
        application_name: None,
        application_website: None,
        replies_count: 0,
        reblogs_count: 0,
        favourites_count: 0,
        quotes_count: 0,
        edited_at: None,
        deleted_at: None,
        expires_at: None,
        inserted_at: stamped(),
        updated_at: stamped(),
    }
}

/// A stored attachment row with `url` and `content_type` set.
pub(super) fn attachment(url: &str, content_type: &str) -> AttachmentRow {
    AttachmentRow {
        id: Uuid::now_v7(),
        user_id: Some(Uuid::parse_str(AUTHOR_ID).unwrap()),
        status_id: Some(Uuid::nil()),
        position: 0,
        storage_key: Some("media/x.png".to_owned()),
        remote_url: None,
        url: url.to_owned(),
        preview_url: None,
        content_type: content_type.to_owned(),
        description: None,
        blurhash: None,
        width: None,
        height: None,
        preview_width: None,
        preview_height: None,
        duration: None,
        focus_x: None,
        focus_y: None,
        file_size: None,
        sensitive_hint: false,
        inserted_at: stamped(),
        updated_at: stamped(),
    }
}

/// A stored local custom emoji row named `shortcode`, in the `cats` pack,
/// with a still variant.
pub(super) fn emoji_row(shortcode: &str) -> EmojiRow {
    EmojiRow {
        id: Uuid::now_v7(),
        shortcode: shortcode.to_owned(),
        domain: None,
        image_url: format!("{BASE}/emoji/cats/{shortcode}.gif"),
        static_url: Some(format!("{BASE}/emoji/cats/{shortcode}.png")),
        category: Some("cats".to_owned()),
        tags: Vec::new(),
        visible_in_picker: true,
        disabled: false,
        inserted_at: stamped(),
        updated_at: stamped(),
    }
}

/// A stored tag row named `name`.
pub(super) fn hashtag(name: &str) -> Hashtag {
    Hashtag {
        id: Uuid::now_v7(),
        name: name.to_owned(),
        display_name: None,
        inserted_at: stamped(),
    }
}
