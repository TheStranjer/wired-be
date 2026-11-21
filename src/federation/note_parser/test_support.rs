//! The fixtures the `note_parser` children's tests share: the anchor
//! instant, the stored author mirror, the JSON document fixtures and the
//! builders that assemble them. Test-only.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    models::user::{DEFAULT_SCOPE, Flag, User},
    util::time::parse_time,
};

use super::{ParsedNote, parse_note};

/// The id every fixture author row carries.
const ACCOUNT_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
/// The actor the fixtures are authored by.
pub(super) const AUTHOR_AP_ID: &str = "https://remote.example/users/bob";
/// The fixture author's followers collection.
pub(super) const FOLLOWERS: &str = "https://remote.example/users/bob/followers";
/// The public recipient, full `ActivityStreams` spelling.
pub(super) const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// The instant every parse is anchored at: 2024-03-01T00:00:00Z.
pub(super) fn now() -> DateTime<Utc> {
    parse_time("2024-03-01T00:00:00Z").unwrap()
}

/// The stored mirror of the fixture author: remote `bob@remote.example`
/// with its domain and followers collection known.
pub(super) fn author() -> User {
    User {
        id: Uuid::parse_str(ACCOUNT_ID).unwrap(),
        nickname: "bob@remote.example".to_owned(),
        ap_id: AUTHOR_AP_ID.to_owned(),
        local: Flag::new(false),
        domain: Some("remote.example".to_owned()),
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
        followers_url: Some(FOLLOWERS.to_owned()),
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
        notification_settings: json!({}),
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
        last_fetched_at: None,
        inserted_at: parse_time("2024-01-31T12:34:56.789Z").unwrap(),
        updated_at: parse_time("2024-01-31T12:34:56.789Z").unwrap(),
        preferred_frontend: None,
        is_suggested: Flag::new(false),
        password_reset_pending: Flag::new(false),
        email_notifications: Flag::new(true),
        deleted_at: None,
    }
}

/// The Mastodon fixture: every field in its common single-value or
/// `Link`-object spelling, hostile markup included.
pub(super) const MASTODON: &str = include_str!("../../../tests/fixtures/ap/mastodon_note.json");
/// The Pleroma fixture: the array, object and bare-field spellings,
/// `contentMap`, `as:Public` and the `ostatus:conversation` context.
pub(super) const PLEROMA: &str = include_str!("../../../tests/fixtures/ap/pleroma_note.json");
/// The `Question` fixture: a finished single-choice poll with counts.
pub(super) const QUESTION: &str = include_str!("../../../tests/fixtures/ap/question.json");
/// The `Article` fixture: a title and body to render.
pub(super) const ARTICLE: &str = include_str!("../../../tests/fixtures/ap/article.json");
/// The Misskey fixture: `_misskey_quote`, `_misskey_content`, a
/// `javascript:` icon and a non-url `url`.
pub(super) const MISSKEY: &str = include_str!("../../../tests/fixtures/ap/misskey_note.json");

/// Reads a fixture string as JSON; a broken fixture fails the test.
pub(super) fn fixture(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

/// Parses `document` by the fixture author at [`now`]; documents the
/// ticket expects to parse.
pub(super) fn parse(document: &Value) -> ParsedNote {
    parse_note(document, &author(), now()).unwrap()
}

/// The smallest document that parses: a public note with one sentence.
pub(super) fn minimal_note() -> Value {
    json!({
        "id": "https://remote.example/objects/minimal",
        "type": "Note",
        "attributedTo": AUTHOR_AP_ID,
        "to": [PUBLIC],
        "cc": [FOLLOWERS],
        "content": "<p>hello</p>",
    })
}

/// Removes `field` from the document's top-level object.
pub(super) fn remove(document: &mut Value, field: &str) {
    let _gone = document.as_object_mut().unwrap().remove(field);
}
