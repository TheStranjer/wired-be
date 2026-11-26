//! The unit-test fixtures the account children share: the stored rows the
//! renders are measured against — a freshly federated remote mirror at the
//! column defaults, a local account with its Pleroma columns set, and a
//! second account to act as a viewer who is not the rendered one — the
//! render-to-JSON helpers, the media-proxy config, and the key sets a
//! response must match exactly.

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{Account, JsonBool};
use crate::{
    config::{Config, TEST_BASE_URL, TEST_DATABASE_URL},
    entities::relationship::{Relationship, RelationshipMap},
    models::{
        custom_emoji::CustomEmoji as EmojiRow,
        user::{DEFAULT_SCOPE, Flag, User, notification_settings::NotificationSettings},
    },
    util::time::parse_time,
};

// The rendered object's keys, sorted, for exact key-set assertions.
pub(super) use crate::test_fixtures::json_keys;

/// The hyphenated lowercase id every fixture row carries.
pub(super) const ACCOUNT_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
/// A second id, for a fixture account that must not be the same row as
/// [`ACCOUNT_ID`].
pub(super) const OTHER_ACCOUNT_ID: &str = "01934567-89ab-7cde-f012-3456789abcdf";
/// A third id, for the staff viewer fixtures.
pub(super) const STAFF_ACCOUNT_ID: &str = "01934567-89ab-7cde-f012-3456789abce0";
/// The instance origin of [`Config::for_tests`].
pub(super) const BASE: &str = "https://test.example";
/// Every JSON key the core entity must emit, sorted.
pub(super) const EXPECTED_KEYS: [&str; 27] = [
    "acct",
    "avatar",
    "avatar_static",
    "bot",
    "created_at",
    "discoverable",
    "display_name",
    "emojis",
    "fields",
    "followers_count",
    "following_count",
    "fqn",
    "group",
    "header",
    "header_static",
    "id",
    "last_status_at",
    "limited",
    "locked",
    "moved",
    "note",
    "pleroma",
    "statuses_count",
    "suspended",
    "uri",
    "url",
    "username",
];
/// The `pleroma` keys a viewer who is neither the account nor staff
/// sees: without the seven owner-only keys, without `birthday` when the
/// account hides it, and without `allow_following_move`, `deactivated`
/// and `tags`.
pub(super) const PLEROMA_KEYS_FOR_OTHERS: [&str; 14] = [
    "accepts_chat_messages",
    "ap_id",
    "background_image",
    "confirmation_pending",
    "favicon",
    "hide_favorites",
    "hide_followers",
    "hide_followers_count",
    "hide_follows",
    "hide_follows_count",
    "is_admin",
    "is_moderator",
    "relationship",
    "skip_thread_containment",
];
/// The `pleroma` keys a staff viewer sees on another account: the list
/// above plus `deactivated` and `tags`, sorted.
pub(super) const PLEROMA_KEYS_FOR_STAFF: [&str; 16] = [
    "accepts_chat_messages",
    "ap_id",
    "background_image",
    "confirmation_pending",
    "deactivated",
    "favicon",
    "hide_favorites",
    "hide_followers",
    "hide_followers_count",
    "hide_follows",
    "hide_follows_count",
    "is_admin",
    "is_moderator",
    "relationship",
    "skip_thread_containment",
    "tags",
];
/// Every `pleroma` key the account sees on itself — the public list, the
/// seven owner-only keys, `birthday`, `allow_following_move` and `tags`,
/// but not `deactivated` (staff only) — sorted for key-set assertions.
pub(super) const PLEROMA_KEYS_FOR_OWNER: [&str; 23] = [
    "accepts_chat_messages",
    "allow_following_move",
    "ap_id",
    "background_image",
    "birthday",
    "chat_token",
    "confirmation_pending",
    "favicon",
    "hide_favorites",
    "hide_followers",
    "hide_followers_count",
    "hide_follows",
    "hide_follows_count",
    "is_admin",
    "is_moderator",
    "notification_settings",
    "relationship",
    "settings_store",
    "show_birthday",
    "skip_thread_containment",
    "tags",
    "unread_conversation_count",
    "unread_notifications_count",
];

/// 2024-01-31T12:34:56.789Z, the instant every fixture row is
/// inserted and last updated at.
pub(super) fn stamped() -> DateTime<Utc> {
    parse_time("2024-01-31T12:34:56.789Z").unwrap()
}

/// A stored custom emoji row named `shortcode` of `domain` (`None` is the
/// local set), in the `cats` pack, with a still variant.
pub(super) fn emoji_row(shortcode: &str, domain: Option<&str>) -> EmojiRow {
    EmojiRow {
        id: Uuid::now_v7(),
        shortcode: shortcode.to_owned(),
        domain: domain.map(str::to_owned),
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

/// A [`User`] row named `nickname` with actor id `ap_id`, at the field
/// defaults a freshly federated mirror carries: a remote `Person`, no
/// images, no profile url, empty text, zero counts, active.
pub(super) fn base_user(nickname: &str, ap_id: &str) -> User {
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
        fields: serde_json::json!([]),
        raw_fields: serde_json::json!([]),
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
        notification_settings: NotificationSettings::default_value(),
        mfa_settings: crate::models::user::mfa::MfaSettings::default_value(),
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
        remote_suspended: Flag::new(false),
    }
}

/// `user` rendered to a JSON value; these structs always serialize.
pub(super) fn render(user: &User, config: &Config) -> Value {
    serde_json::to_value(Account::from_user(user, config)).unwrap()
}

/// `user` rendered as `viewer` sees it (`None` is anonymous), without
/// any preloaded relationships.
pub(super) fn render_for(viewer: Option<&User>, user: &User, config: &Config) -> Value {
    render_for_with(viewer, user, config, None)
}

/// `user` rendered as `viewer` sees it with `relationships` as the
/// optional preload for the embedded `pleroma.relationship`.
pub(super) fn render_for_with(
    viewer: Option<&User>,
    user: &User,
    config: &Config,
    relationships: Option<&RelationshipMap>,
) -> Value {
    serde_json::to_value(Account::from_user_for(viewer, user, config, relationships)).unwrap()
}

/// The `pleroma` block of a rendered account, owned so a test can bind
/// it in the same statement it renders in.
pub(super) fn pleroma_of(value: &Value) -> Value {
    value
        .get("pleroma")
        .cloned()
        .expect("every account renders pleroma")
}

/// The `pleroma` block a row at the migration defaults renders to an
/// anonymous viewer: the public keys only, with `ap_id` and the chat
/// capability (`accepts_chat`) as given.
pub(super) fn pleroma_block(ap_id: &str, accepts_chat: Option<bool>) -> Value {
    json!({
        "background_image": null,
        "relationship": {},
        "is_admin": false,
        "is_moderator": false,
        "confirmation_pending": false,
        "hide_favorites": true,
        "hide_followers": false,
        "hide_follows": false,
        "hide_followers_count": false,
        "hide_follows_count": false,
        "accepts_chat_messages": accepts_chat,
        "favicon": null,
        "skip_thread_containment": false,
        "ap_id": ap_id,
    })
}

/// A local admin viewer, distinct from [`alice_with_pleroma_profile`] and
/// [`mallory`], for the staff-only keys.
pub(super) fn staff_admin() -> User {
    let mut admin = base_user("root", &format!("{BASE}/users/root"));
    admin.id = Uuid::parse_str(STAFF_ACCOUNT_ID).unwrap();
    admin.local = Flag::new(true);
    admin.is_admin = Flag::new(true);
    admin
}

/// A local moderator viewer, the other half of staff.
pub(super) fn staff_moderator() -> User {
    let mut moderator = staff_admin();
    moderator.nickname = "mod".to_owned();
    moderator.ap_id = format!("{BASE}/users/mod");
    moderator.is_admin = Flag::new(false);
    moderator.is_moderator = Flag::new(true);
    moderator
}

/// A local `alice` whose Pleroma columns are all set: profile tags, a
/// background image, a birthday, a stored client-settings blob and a
/// `block_from_strangers` flag stored without its sibling key (which
/// the entity renders back at its default, proving the canonical
/// shape).
pub(super) fn alice_with_pleroma_profile() -> User {
    let mut alice = base_user("alice", &format!("{BASE}/users/alice"));
    alice.local = Flag::new(true);
    alice.tags = vec!["rust".to_owned(), "gnw".to_owned()];
    alice.background_image_url = Some(format!("{BASE}/bg.png"));
    alice.birthday = NaiveDate::from_ymd_opt(1990, 1, 31);
    alice.settings_store = json!({"pleroma_fe": {"theme": "dark"}});
    alice.notification_settings = json!({"block_from_strangers": true});
    alice
}

/// A second account to act as a viewer who is not the rendered one:
/// its own primary key, since being the owner is decided by id.
pub(super) fn mallory() -> User {
    let mut stranger = base_user(
        "mallory@remote.example",
        "https://remote.example/users/mallory",
    );
    stranger.id = Uuid::parse_str(OTHER_ACCOUNT_ID).unwrap();
    stranger
}

/// A signing secret comfortably past the 32-character minimum.
pub(super) const TEST_PROXY_SECRET: &str = "0123456789abcdef0123456789abcdef01234567";

/// A `Config` on the test origin with the media proxy enabled.
pub(super) fn proxy_config() -> Config {
    Config::from_lookup(|key| match key {
        crate::config::DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
        crate::config::BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
        crate::config::MEDIA_PROXY_ENABLED_VAR => Some("true".to_owned()),
        crate::config::MEDIA_PROXY_SECRET_VAR => Some(TEST_PROXY_SECRET.to_owned()),
        _ => None,
    })
    .unwrap()
}

/// A remote `bob@remote.example` mirroring the images federation stored.
pub(super) fn remote_user_with_images() -> User {
    let mut bob = base_user("bob@remote.example", "https://remote.example/users/bob");
    bob.domain = Some("remote.example".to_owned());
    bob.avatar_url = Some("https://remote.example/avatar.png".to_owned());
    bob.banner_url = Some("https://remote.example/banner.gif".to_owned());
    bob
}

/// A loaded relationship for `target`: the follow-derived flags a
/// viewer who follows them with notifications on and reblogs hidden
/// would have; the rest at the constructor's defaults.
pub(super) fn following_relationship(target: &User) -> Relationship {
    Relationship {
        following: JsonBool(true),
        notifying: JsonBool(true),
        subscribing: JsonBool(true),
        showing_reblogs: JsonBool(false),
        ..Relationship::unconnected(target.id)
    }
}

/// `user` rendered by [`Account::for_owner`] — the
/// `verify_credentials` view — with `requests` pending follow requests.
pub(super) fn render_for_owner(user: &User, config: &Config, requests: i32) -> Value {
    serde_json::to_value(Account::for_owner(user, config, requests)).unwrap()
}

/// The `source` block of a rendered account, owned so a test can bind
/// it in the same statement it renders in.
pub(super) fn source_of(value: &Value) -> Value {
    value
        .get("source")
        .cloned()
        .expect("source block is rendered")
}

/// The key set of a value, for the presence assertions below.
pub(super) fn has_key(value: &Value, key: &str) -> bool {
    value.get(key).is_some()
}
