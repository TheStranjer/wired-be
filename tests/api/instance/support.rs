//! The shared fixtures of the instance tests: the expected documents, the
//! config variant and the helpers that mirror remotes and set the MRF chain.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::config::{Config, Toggle};

use crate::common;

/// The instance origin of [`common::test_config`].
pub(super) const BASE: &str = "https://test.example";

/// The feature list Pleroma 2.10.2 advertises with its shipped defaults,
/// spelled out here (as in `tests/nodeinfo.rs`) so the shared builder is
/// checked against the ticket, not echoed by it.
pub(super) const EXPECTED_FEATURES: [&str; 26] = [
    "pleroma_api",
    "mastodon_api",
    "mastodon_api_grouped_notifications",
    "mastodon_api_streaming",
    "polls",
    "v2_suggestions",
    "pleroma_explicit_addressing",
    "shareable_emoji_packs",
    "multifetch",
    "pleroma:api/v1/notifications:include_types_filter",
    "editing",
    "quote_posting",
    "blockers_visible",
    "chat",
    "shout",
    "relay",
    "pleroma_emoji_reactions",
    "pleroma_custom_emoji_reactions",
    "pleroma_chat_messages",
    "pleroma:pin_chats",
    "exposable_reactions",
    "profile_directory",
    "pleroma:group_actors",
    "pleroma:bookmark_folders",
    "pleroma:block_expiration",
    "wired:protocols",
];

/// The sorted top-level keys of the instance document — the ticket's key
/// set.
pub(super) const TOP_KEYS: [&str; 26] = [
    "approval_required",
    "avatar_upload_limit",
    "background_upload_limit",
    "banner_upload_limit",
    "configuration",
    "contact_account",
    "description",
    "description_limit",
    "email",
    "invites_enabled",
    "languages",
    "max_media_attachments",
    "max_toot_chars",
    "pleroma",
    "poll_limits",
    "registrations",
    "rules",
    "short_description",
    "stats",
    "thumbnail",
    "title",
    "upload_limit",
    "uri",
    "urls",
    "version",
    "wired",
];

/// The `version` string the endpoint claims: the Mastodon API level,
/// commented with the Pleroma release Wired is compatible with and
/// Wired's own crate version.
pub(super) fn instance_version() -> String {
    format!(
        "2.7.2 (compatible; Pleroma 2.10.2; Wired {})",
        env!("CARGO_PKG_VERSION")
    )
}

/// The instance VAPID key the documents advertise: reading it seeds the
/// pair if the test's database has none, exactly as the endpoints do.
pub(super) async fn instance_vapid_key(pool: &PgPool) -> String {
    wired_be::services::vapid::vapid_public_key(pool)
        .await
        .expect("the instance VAPID key is readable")
}

/// The `configuration` block of the empty [`test_config`] document: the
/// configured defaults beside the shipped constants.
pub(super) fn expected_configuration() -> Value {
    json!({
        "statuses": {
            "max_characters": 5000,
            "max_media_attachments": 4,
            "characters_reserved_per_url": 23,
        },
        "media_attachments": {
            "supported_mime_types": ["application/octet-stream"],
            "image_size_limit": 16_000_000,
            "video_size_limit": 16_000_000,
            "image_matrix_limit": 16_777_216,
            "video_matrix_limit": 2_304_000,
            "video_frame_rate_limit": 60,
        },
        "polls": {
            "max_options": 20,
            "max_characters_per_option": 200,
            "min_expiration": 0,
            "max_expiration": 31_536_000,
        },
        "accounts": { "max_featured_tags": 10 },
    })
}

/// The `pleroma` block of the empty [`test_config`] document: the shared
/// capability lists beside Pleroma's shipped metadata.
pub(super) fn expected_pleroma(vapid_key: &str) -> Value {
    json!({
        "metadata": {
            "account_activation_required": false,
            "birthday_required": false,
            "birthday_min_age": 0,
            "features": EXPECTED_FEATURES,
            "federation": {
                "enabled": true,
                "mrf_policies": ["HashtagPolicy"],
                "mrf_hashtag": {
                    "sensitive": ["nsfw"],
                    "reject": [],
                    "federated_timeline_removal": [],
                },
                "quarantined_instances": [],
                "exclusions": false,
            },
            "fields_limits": {
                "max_fields": 10,
                "max_remote_fields": 20,
                "name_length": 512,
                "value_length": 2048,
            },
            "post_formats": ["text/plain", "text/html", "text/markdown"],
            "restrict_unauthenticated": {
                "timelines": { "local": false, "federated": false },
                "profiles": { "local": false, "remote": false },
                "activities": { "local": false, "remote": false },
            },
            "markup": {
                "allow_inline_images": true,
                "allow_headings": true,
                "allow_tables": false,
                "allow_fonts": false,
            },
        },
        "stats": { "mau": 0 },
        "vapid_public_key": vapid_key,
    })
}

/// The complete document an empty instance built on [`common::test_config`]
/// serves: zeroed stats, no rules, no contact account, and every limit at
/// its configured default or shipped constant.
pub(super) fn expected_document(vapid_key: &str) -> Value {
    json!({
        "uri": "test.example",
        "title": "Wired",
        "short_description": "A Wired instance, compatible with Pleroma clients",
        "description": "A Wired instance, compatible with Pleroma clients",
        "email": "",
        "version": instance_version(),
        "urls": { "streaming_api": "wss://test.example" },
        "stats": { "user_count": 0, "status_count": 0, "domain_count": 0 },
        "thumbnail": format!("{BASE}/media/default/thumbnail.jpg"),
        "languages": ["en"],
        "registrations": true,
        "approval_required": false,
        "invites_enabled": false,
        "contact_account": null,
        "rules": [],
        "max_toot_chars": 5000,
        "max_media_attachments": 4,
        "poll_limits": {
            "max_options": 20,
            "max_option_chars": 200,
            "min_expiration": 0,
            "max_expiration": 31_536_000,
        },
        "upload_limit": 16_000_000,
        "avatar_upload_limit": 2_000_000,
        "background_upload_limit": 4_000_000,
        "banner_upload_limit": 4_000_000,
        "description_limit": 5000,
        "configuration": expected_configuration(),
        "pleroma": expected_pleroma(vapid_key),
        "wired": { "protocols": ["activitypub"] },
    })
}

/// `GET /api/v1/instance` and require a 200 JSON document.
pub(super) async fn get_instance(app: &axum::Router) -> Value {
    let response = common::TestRequest::get("/api/v1/instance").send(app).await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the instance document is served"
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the document is JSON"
    );
    response.json_value()
}

/// The sorted object keys of `value`, for key-set assertions.
pub(super) fn keys_of(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect()
}

/// A [`common::test_config`] with every setting the document mirrors set
/// away from its default, over a plain-http origin with a port.
pub(super) fn other_config() -> Config {
    let mut config = common::test_config();
    config.instance_name = "Snatter".to_owned();
    config.instance_description = "Where we all chatter".to_owned();
    config.contact_email = Some("admin@example.org".to_owned());
    config.instance_languages = vec!["en".to_owned(), "de".to_owned()];
    config.base_url = "http://snatter.example:4000".to_owned();
    config.local_domain = "snatter.example:4000".to_owned();
    config.media_base_url = "http://snatter.example:4000/media".to_owned();
    config.max_status_chars = 300;
    config.max_media_attachments = 6;
    config.max_poll_options = 12;
    config.upload_limits.max_file_bytes = 1_048_576;
    config.registrations_open = Toggle::new(false);
    config.invites_enabled = Toggle::new(true);
    config
}

/// Inserts `domain`'s mirror so the peer listing has hosts to read.
pub(super) async fn mirror_remote(pool: &PgPool, host: &str) {
    let _user = common::create_remote_user(pool, "someone", host).await;
}

/// Writes the `:mrf` config entry the endpoints and the pipeline both
/// read: the tests configure policy chains through the same row the admin
/// config store owns (WBE-206).
pub(super) async fn set_mrf(pool: &PgPool, setting: &Value) {
    wired_be::services::runtime_config::write(pool, ":pleroma", ":mrf", setting)
        .await
        .expect("the mrf entry writes");
}

/// The `simple` lists the moderation tests configure: one host per
/// severity arm of the mapping, one wildcard, one reason.
pub(super) const SIMPLE_LISTS: &str = r#"{
    "policies": ["simple"],
    "simple": {
        "reject": [["blocked.example", "coordinated abuse"], "*.worse.example"],
        "federated_timeline_removal": ["noisy.example"],
        "followers_only": ["press.example"],
        "media_removal": ["leech.example"],
        "media_nsfw": [["lewd.example", "no nsfw"]],
        "avatar_removal": ["faceless.example"]
    }
}"#;
