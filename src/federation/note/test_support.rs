//! The shared fixtures the `note` children's tests build on: the instance
//! constants, the user, status and [`NoteContext`] factories, the note
//! renderer and the key-set helper. Test-only.

use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    config::Config,
    models::{
        media_attachment::MediaAttachment,
        status::{Status, Visibility},
        user::{self, Flag, User},
    },
    util::time::parse_time,
};

use super::{NoteContext, litepub_context_json, note_json};

// Every field of a rendered document, sorted — the exact key-set assertion
// the goldens pair with.
pub(super) use crate::test_fixtures::json_keys;

/// The instance origin of [`Config::for_tests`].
pub(super) const BASE: &str = "https://test.example";
/// The schema URL every document's `@context` names.
const SCHEMA_URL: &str = "https://test.example/schemas/litepub-0.1.jsonld";
/// The author fixture's id, a `UUIDv7` shape spelled out for equality.
pub(super) const AUTHOR_ID: &str = "01934567-89ab-7cde-f012-3456789abcde";
/// The author's actor id.
pub(super) const AUTHOR_AP: &str = "https://test.example/users/alice";
/// The status fixture's id.
pub(super) const STATUS_ID: &str = "01934568-1111-7000-8000-abcdef012345";
/// The mentioned remote account's actor id.
pub(super) const BOB_AP: &str = "https://remote.example/users/bob";
/// A parent post's object id on a peer instance.
pub(super) const PARENT_URI: &str = "https://remote.example/objects/parent-post";

/// Every fixture's creation moment, in the wire format.
pub(super) const PUBLISHED: &str = "2024-01-31T12:34:56.789Z";
/// The moment the tombstone fixture reports its deletion.
pub(super) const DELETED: &str = "2024-02-01T08:00:00.000Z";

/// A [`User`] row named `nickname` at `ap_id`, local or not as asked:
/// the mention and author fixtures.
fn fixture_user(nickname: &str, ap_id: &str, local: bool) -> User {
    User {
        id: Uuid::parse_str(AUTHOR_ID).unwrap(),
        nickname: nickname.to_owned(),
        ap_id: ap_id.to_owned(),
        local: Flag::new(local),
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
        default_scope: user::DEFAULT_SCOPE.to_owned(),
        sensitive_default: Flag::new(false),
        language: None,
        tags: Vec::new(),
        settings_store: json!({}),
        notification_settings: json!({}),
        mfa_settings: user::mfa::MfaSettings::default_value(),
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
        inserted_at: parse_time(PUBLISHED).unwrap(),
        updated_at: parse_time(PUBLISHED).unwrap(),
        is_suggested: Flag::new(false),
        password_reset_pending: Flag::new(false),
        email_notifications: Flag::new(true),
        deleted_at: None,
    }
}

/// The author: local `alice` at [`AUTHOR_AP`].
pub(crate) fn author() -> User {
    fixture_user("alice", AUTHOR_AP, true)
}

/// A mentioned remote account named `bob`.
pub(super) fn mentioned_bob() -> User {
    let mut bob = fixture_user("bob@remote.example", BOB_AP, false);
    bob.domain = Some("remote.example".to_owned());
    bob
}

/// The status fixture: a local `public` post of "hello" whose id, uri,
/// url and conversation are all the constants above.
pub(super) fn base_status(visibility: Visibility) -> Status {
    Status {
        id: Uuid::parse_str(STATUS_ID).unwrap(),
        uri: format!("{BASE}/objects/{STATUS_ID}"),
        url: Some(format!("{BASE}/notice/{STATUS_ID}")),
        user_id: Uuid::parse_str(AUTHOR_ID).unwrap(),
        content: "<p>hello</p>".to_owned(),
        raw_text: "hello".to_owned(),
        content_type: "text/plain".to_owned(),
        spoiler_text: String::new(),
        sensitive: false,
        visibility,
        language: None,
        in_reply_to_id: None,
        in_reply_to_account_id: None,
        in_reply_to_ap_id: None,
        conversation_id: Uuid::parse_str(STATUS_ID).unwrap(),
        ap_context: None,
        direct_conversation_id: None,
        reblog_of_id: None,
        quote_of_id: None,
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
        inserted_at: parse_time(PUBLISHED).unwrap(),
        updated_at: parse_time(PUBLISHED).unwrap(),
    }
}

/// A [`NoteContext`] for a bare status of `visibility`: no satellites,
/// the test instance's config.
pub(super) fn note_context(visibility: Visibility) -> NoteContext {
    NoteContext {
        status: base_status(visibility),
        author: author(),
        reply_parent_uri: None,
        mentions: Vec::new(),
        hashtags: Vec::new(),
        attachments: Vec::new(),
        emojis: Vec::new(),
        poll: None,
        quote_uri: None,
        config: Config::for_tests(),
    }
}

/// Render the bare note of `visibility`, expecting the refusal of a
/// `local` one to be tested separately.
pub(super) fn render(visibility: Visibility) -> Value {
    let ctx = note_context(visibility);
    note_json(&ctx).expect("a federatable visibility renders")
}

/// The `@context` every document of the test instance carries.
pub(super) fn expected_context() -> Value {
    json!([
        "https://www.w3.org/ns/activitystreams",
        SCHEMA_URL,
        { "@language": "und" },
    ])
}

/// Every key a bare note object carries.
pub(super) const NOTE_KEYS: [&str; 19] = [
    "@context",
    "actor",
    "attachment",
    "attributedTo",
    "cc",
    "content",
    "context",
    "conversation",
    "id",
    "inReplyTo",
    "interactionPolicy",
    "published",
    "sensitive",
    "source",
    "summary",
    "tag",
    "to",
    "type",
    "url",
];

/// Terms of the `ActivityStreams` and `security` vocabularies the first two
/// `@context` entries define, so the served schema does not have to.
const CORE_TERMS: [&str; 36] = [
    "actor",
    "anyOf",
    "attachment",
    "attributedTo",
    "cc",
    "closed",
    "content",
    "context",
    "deleted",
    "endTime",
    "endpoints",
    "followers",
    "height",
    "href",
    "following",
    "icon",
    "id",
    "image",
    "inReplyTo",
    "inbox",
    "mediaType",
    "name",
    "object",
    "oneOf",
    "outbox",
    "preferredUsername",
    "publicKey",
    "published",
    "source",
    "summary",
    "tag",
    "to",
    "type",
    "updated",
    "url",
    "width",
];

/// A media row at `url` of `content_type`: no description, no blurhash and
/// no geometry, the attachment fixture the builders' tests start from.
pub(crate) fn fixture_media(url: &str, content_type: &str) -> MediaAttachment {
    MediaAttachment {
        id: Uuid::parse_str(STATUS_ID).unwrap(),
        user_id: Some(Uuid::parse_str(AUTHOR_ID).unwrap()),
        status_id: Some(Uuid::parse_str(STATUS_ID).unwrap()),
        position: 0,
        storage_key: None,
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
        inserted_at: parse_time(PUBLISHED).unwrap(),
        updated_at: parse_time(PUBLISHED).unwrap(),
    }
}

/// Every key `value` writes that neither a core vocabulary nor the served
/// [`litepub_context_json`] defines, sorted and deduplicated: the top-level
/// keys of the document and the keys of its `attachment` objects (where
/// `blurhash` lives). JSON-LD keywords (`@context`) are not terms.
pub(crate) fn undefined_terms(document: &Value) -> Vec<String> {
    let schema = litepub_context_json();
    let defined = schema["@context"]
        .as_object()
        .expect("the schema is a term map");
    let mut keys: Vec<&str> = Vec::new();
    if let Some(fields) = document.as_object() {
        keys.extend(fields.keys().map(String::as_str));
    }
    let attachments = match document.get("attachment") {
        Some(Value::Array(entries)) => entries.iter().collect(),
        Some(single) => vec![single],
        None => Vec::new(),
    };
    for entry in attachments {
        if let Some(fields) = entry.as_object() {
            keys.extend(fields.keys().map(String::as_str));
        }
    }
    let mut missing: Vec<String> = keys
        .into_iter()
        .filter(|key| {
            !key.starts_with('@') && !CORE_TERMS.contains(key) && !defined.contains_key(*key)
        })
        .map(str::to_owned)
        .collect();
    missing.sort_unstable();
    missing.dedup();
    missing
}
