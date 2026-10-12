//! Tests for `GET /api/v1/instance` (`src/api/instance.rs`).
//!
//! The endpoint is public (Pleroma's `skip_auth`) and takes no
//! parameters, so there is no auth failure, 404 or validation arm to
//! cover: what these tests own is the exact key set and value types of
//! the document, the three cached counters against local/remote
//! fixtures, the priority-ordered rules, the contact account (present
//! and absent), the config mirroring through non-default settings, and
//! the 60-second stats cache window. The cache helper's own expiry
//! arithmetic is unit-tested where it lives, in `src/util/cache.rs`.

#![cfg(test)]

// `pub` because Clippy's nursery `redundant_pub_crate` forbids `pub(crate)`
// items inside a private module, while `unreachable_pub` forbids `pub` ones:
// only a `pub` module makes the harness helpers lint-clean.
pub mod common;

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use wired_be::{
    config::{Config, Toggle},
    entities::account::Account,
    models::{
        instance_rule,
        status::{self, NewStatus, Visibility},
        user::NewUser,
    },
    services::instance_meta,
};

/// The instance origin of [`common::test_config`].
const BASE: &str = "https://test.example";

/// The feature list Pleroma 2.10.2 advertises with its shipped defaults,
/// spelled out here (as in `tests/nodeinfo.rs`) so the shared builder is
/// checked against the ticket, not echoed by it.
const EXPECTED_FEATURES: [&str; 26] = [
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
    "pleroma:get:main/ostatus",
    "pleroma:group_actors",
    "pleroma:bookmark_folders",
    "pleroma:block_expiration",
];

/// The sorted top-level keys of the instance document — the ticket's key
/// set.
const TOP_KEYS: [&str; 25] = [
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
];

/// The `version` string the endpoint claims: the Mastodon API level,
/// commented with the replicated Pleroma release and this crate.
fn instance_version() -> String {
    format!(
        "2.7.2 (compatible; Pleroma 2.10.2 wired-be {})",
        env!("CARGO_PKG_VERSION")
    )
}

/// The `configuration` block of the empty [`test_config`] document: the
/// configured defaults beside the shipped constants.
fn expected_configuration() -> Value {
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
fn expected_pleroma() -> Value {
    json!({
        "metadata": {
            "account_activation_required": false,
            "birthday_required": false,
            "birthday_min_age": 0,
            "features": EXPECTED_FEATURES,
            "federation": {
                "enabled": true,
                "mrf_policies": [],
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
                "allow_inline_images": false,
                "allow_headings": false,
                "allow_tables": false,
                "allow_fonts": false,
            },
        },
        "stats": { "mau": 0 },
        "vapid_public_key": null,
    })
}

/// The complete document an empty instance built on [`common::test_config`]
/// serves: zeroed stats, no rules, no contact account, and every limit at
/// its configured default or shipped constant.
fn expected_document() -> Value {
    json!({
        "uri": "test.example",
        "title": "Wired",
        "short_description": "A Pleroma-compatible instance",
        "description": "A Pleroma-compatible instance",
        "email": "",
        "version": instance_version(),
        "urls": { "streaming_api": "wss://test.example" },
        "stats": { "user_count": 0, "status_count": 0, "domain_count": 0 },
        "thumbnail": format!("{BASE}/images/thumbnail.jpg"),
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
        "pleroma": expected_pleroma(),
    })
}

/// `GET /api/v1/instance` and require a 200 JSON document.
async fn get_instance(app: &axum::Router) -> Value {
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
fn keys_of(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect()
}

/// The empty instance serves the ticket's complete document — every key,
/// type and default at once — without any authentication: a garbage
/// bearer token is not even looked at, and a wrong method is the plain
/// 405 axum gives for a known path.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_document_is_public_and_complete(pool: PgPool) {
    let app = common::test_app(pool);
    let document = get_instance(&app).await;
    assert_eq!(
        keys_of(&document),
        TOP_KEYS,
        "the ticket's top-level key set"
    );
    assert_eq!(document, expected_document(), "the whole document");
    assert_eq!(
        document["pleroma"]["metadata"]["features"],
        json!(instance_meta::features()),
        "the feature list is the shared NodeInfo builder, not a restatement"
    );
    let with_garbage_auth = common::TestRequest::get("/api/v1/instance")
        .bearer("not-a-real-token")
        .send(&app)
        .await;
    assert_eq!(
        with_garbage_auth.status,
        StatusCode::OK,
        "credentials are never consulted"
    );
    assert_eq!(
        with_garbage_auth.json_value(),
        document,
        "and the answer is the same document"
    );
    let posted = common::TestRequest::post("/api/v1/instance")
        .send(&app)
        .await;
    assert_eq!(
        posted.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "the endpoint is read-only"
    );
}

/// The counters count active local accounts, live non-reblog posts this
/// instance owns, and the distinct hosts of the mirrored remote actors:
/// deactivated locals, reblogs, tombstones, remote posts and duplicate
/// domains each drop out exactly as the ticket defines them.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn stats_count_local_users_local_posts_and_remote_domains(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let carol = common::create_local_user(&pool, "carol").await;
    let _erin = common::create_local_user_with(&pool, "erin", |user| NewUser {
        is_active: false.into(),
        ..user
    })
    .await;
    let fern = common::create_remote_user(&pool, "fern", "remote.example").await;
    let _gap = common::create_remote_user(&pool, "gap", "remote.example").await;
    let _hank = common::create_remote_user(&pool, "hank", "other.example").await;

    let _one = common::create_status(&pool, &alice, "one", Visibility::Public).await;
    let _two = common::create_status(&pool, &carol, "two", Visibility::Public).await;
    let _three = common::create_status(&pool, &carol, "three", Visibility::Local).await;
    let doomed = common::create_status(&pool, &alice, "gone", Visibility::Public).await;
    let _tombstone = status::soft_delete(&pool, doomed.id)
        .await
        .expect("the tombstone write runs");
    let original = common::create_status(&pool, &alice, "hers", Visibility::Public).await;
    let reblog = NewStatus {
        reblog_of_id: Some(original.id),
        ..NewStatus::local(&carol, "")
    };
    let _reblogged = status::insert(&pool, &common::test_config().base_url, &reblog)
        .await
        .expect("the reblog inserts");
    let mirrored = NewStatus {
        uri: Some("https://remote.example/posts/1".to_owned()),
        user_id: fern.id,
        content: "a foreign post".to_owned(),
        ..NewStatus::default()
    };
    let _mirrored = status::insert(&pool, &common::test_config().base_url, &mirrored)
        .await
        .expect("the remote status inserts");

    let app = common::test_app(pool);
    let document = get_instance(&app).await;
    assert_eq!(
        document["stats"],
        json!({ "user_count": 2, "status_count": 4, "domain_count": 2 }),
        "alice and carol post, erin is deactivated, the reblog and tombstone \
         drop out, the foreign post is not ours, and remote.example counts once"
    );
}

/// The rules arrive in display order: lowest priority first, and within
/// one priority oldest first on the id (decision 4), each rendered with
/// its string id, its text and its hint.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn rules_are_ordered_by_priority_then_id(pool: PgPool) {
    let first = instance_rule::create(&pool, "Be nice", "", 0)
        .await
        .expect("the first rule inserts");
    let third = instance_rule::create(&pool, "No spam", "we delete it", 10)
        .await
        .expect("the third rule inserts");
    let second = instance_rule::create(&pool, "Stay on topic", "", 5)
        .await
        .expect("the second rule inserts");
    let tied = instance_rule::create(&pool, "Second nice rule", "hint", 0)
        .await
        .expect("the tied rule inserts");

    let app = common::test_app(pool);
    let document = get_instance(&app).await;
    assert_eq!(
        document["rules"],
        json!([
            {"id": first.id.to_string(), "text": "Be nice", "hint": ""},
            {"id": tied.id.to_string(), "text": "Second nice rule", "hint": "hint"},
            {"id": second.id.to_string(), "text": "Stay on topic", "hint": ""},
            {"id": third.id.to_string(), "text": "No spam", "hint": "we delete it"},
        ]),
        "priority decides, creation order breaks ties, hints round-trip"
    );
}

/// `contact_account` is `null` while no admin exists, then the first
/// active local admin as a full `Account` entity: moderators, remote
/// admins and deactivated admins never take the slot, and retiring the
/// first admin hands it to the second.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn contact_account_is_the_first_active_local_admin(pool: PgPool) {
    let _mod = common::create_local_user_with(&pool, "mod", |user| NewUser {
        is_moderator: true.into(),
        ..user
    })
    .await;
    let _remote_admin = common::create_remote_user(&pool, "boss", "remote.example").await;
    let flagged = sqlx::query("UPDATE users SET is_admin = true WHERE nickname = $1")
        .bind("boss@remote.example")
        .execute(&pool)
        .await
        .expect("the remote admin flag writes");
    assert_eq!(
        flagged.rows_affected(),
        1,
        "the remote row carries the flag"
    );

    let app = common::test_app(pool.clone());
    let no_contact = get_instance(&app).await;
    assert_eq!(
        no_contact["contact_account"],
        Value::Null,
        "a moderator and a remote admin are not contacts"
    );

    let first = common::create_local_user_with(&pool, "root_one", |user| NewUser {
        is_admin: true.into(),
        ..user
    })
    .await;
    let second = common::create_local_user_with(&pool, "root_two", |user| NewUser {
        is_admin: true.into(),
        ..user
    })
    .await;
    let config = common::test_config();
    let with_admins = get_instance(&common::test_app(pool.clone())).await;
    assert_eq!(
        with_admins["contact_account"],
        serde_json::to_value(Account::from_user(&first, &config)).unwrap(),
        "the first admin (lowest id) renders as the full Account entity"
    );

    let retired = sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(first.id)
        .execute(&pool)
        .await
        .expect("the deactivation writes");
    assert_eq!(retired.rows_affected(), 1, "the first admin is retired");
    let after_retirement = get_instance(&common::test_app(pool)).await;
    assert_eq!(
        after_retirement["contact_account"]["id"],
        Value::String(second.id.to_string()),
        "the next active admin takes the contact slot"
    );
}

/// A [`common::test_config`] with every setting the document mirrors set
/// away from its default, over a plain-http origin with a port.
fn other_config() -> Config {
    let mut config = common::test_config();
    config.instance_name = "Snatter".to_owned();
    config.instance_description = "Where we all chatter".to_owned();
    config.contact_email = Some("admin@example.org".to_owned());
    config.instance_languages = vec!["en".to_owned(), "de".to_owned()];
    config.base_url = "http://snatter.example:4000".to_owned();
    config.max_status_chars = 300;
    config.max_media_attachments = 6;
    config.max_poll_options = 12;
    config.upload_limits.max_file_bytes = 1_048_576;
    config.registrations_open = Toggle::new(false);
    config
}

/// The identity, description, email, languages, scheme and registration
/// state all come from the configuration.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn identity_fields_mirror_the_config(pool: PgPool) {
    let app = common::test_app_with_config(pool, other_config());
    let document = get_instance(&app).await;
    assert_eq!(document["uri"], "snatter.example:4000", "the domain");
    assert_eq!(document["title"], "Snatter", "INSTANCE_NAME");
    assert_eq!(
        document["short_description"], "Where we all chatter",
        "INSTANCE_DESCRIPTION is the short one"
    );
    assert_eq!(
        document["description"], "Where we all chatter",
        "and the long one"
    );
    assert_eq!(document["email"], "admin@example.org", "CONTACT_EMAIL");
    assert_eq!(
        document["languages"],
        json!(["en", "de"]),
        "INSTANCE_LANGUAGES"
    );
    assert_eq!(
        document["urls"]["streaming_api"], "ws://snatter.example:4000",
        "an http origin gets the plain ws scheme"
    );
    assert_eq!(
        document["thumbnail"], "http://snatter.example:4000/images/thumbnail.jpg",
        "the thumbnail follows BASE_URL"
    );
    assert_eq!(
        document["registrations"], false,
        "REGISTRATIONS_OPEN=false is advertised"
    );
    assert_eq!(
        document["approval_required"], false,
        "until the registration-modes ticket, never required"
    );
    assert_eq!(document["invites_enabled"], false, "no invites exist");
}

/// The status, attachment, poll and upload limits mirror the configured
/// numbers everywhere they appear, while the per-purpose upload limits
/// and metadata constants keep their shipped values.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn limits_mirror_the_config(pool: PgPool) {
    let app = common::test_app_with_config(pool, other_config());
    let document = get_instance(&app).await;
    assert_eq!(document["max_toot_chars"], 300, "MAX_STATUS_CHARS");
    assert_eq!(
        document["max_media_attachments"], 6,
        "MAX_MEDIA_ATTACHMENTS"
    );
    assert_eq!(document["upload_limit"], 1_048_576, "UPLOAD_LIMIT_BYTES");
    assert_eq!(
        document["poll_limits"],
        json!({
            "max_options": 12,
            "max_option_chars": 200,
            "min_expiration": 0,
            "max_expiration": 31_536_000,
        }),
        "MAX_POLL_OPTIONS in Pleroma's block, window fixed"
    );
    assert_eq!(
        document["configuration"]["statuses"],
        json!({
            "max_characters": 300,
            "max_media_attachments": 6,
            "characters_reserved_per_url": 23,
        }),
        "the status limits mirror the config beside the fixed reserve"
    );
    assert_eq!(
        document["configuration"]["polls"]["max_options"], 12,
        "MAX_POLL_OPTIONS in Mastodon's block"
    );
    assert_eq!(
        document["configuration"]["media_attachments"],
        json!({
            "supported_mime_types": ["application/octet-stream"],
            "image_size_limit": 1_048_576,
            "video_size_limit": 1_048_576,
            "image_matrix_limit": 16_777_216,
            "video_matrix_limit": 2_304_000,
            "video_frame_rate_limit": 60,
        }),
        "the upload limit sizes image and video, the rest ships fixed"
    );
    for key in [
        "avatar_upload_limit",
        "background_upload_limit",
        "banner_upload_limit",
        "description_limit",
    ] {
        assert_eq!(
            document[key],
            expected_document()[key],
            "{key} stays at its shipped constant"
        );
    }
    assert_eq!(
        document["pleroma"],
        expected_pleroma(),
        "the whole pleroma block is config-independent here"
    );
}

/// While the 60-second window is open the `stats` block is served from
/// the in-memory cache — new accounts and posts do not move it — while
/// rules and the contact account keep answering the moment they change.
/// The window arithmetic itself is unit-tested with an injected clock in
/// `src/util/cache.rs`; what this test owns is that the endpoint routes
/// its counters through that cache and nothing else.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn stats_are_cached_while_their_window_is_open(pool: PgPool) {
    let alice = common::create_local_user(&pool, "alice").await;
    let app = common::test_app(pool.clone());

    let first = get_instance(&app).await;
    assert_eq!(first["stats"]["user_count"], 1, "the first read is live");
    assert_eq!(first["contact_account"], Value::Null, "no admin yet");

    let _bob = common::create_local_user(&pool, "bob").await;
    let _post = common::create_status(&pool, &alice, "late", Visibility::Public).await;
    let root = common::create_local_user_with(&pool, "root", |user| NewUser {
        is_admin: true.into(),
        ..user
    })
    .await;
    let rule = instance_rule::create(&pool, "Late rule", "", 0)
        .await
        .expect("the late rule inserts");

    let second = get_instance(&app).await;
    assert_eq!(
        second["stats"], first["stats"],
        "the cached counters do not see the new account or post"
    );
    assert_eq!(
        second["rules"],
        json!([{"id": rule.id.to_string(), "text": "Late rule", "hint": ""}]),
        "rules are read fresh on every request"
    );
    assert_eq!(
        second["contact_account"]["id"],
        Value::String(root.id.to_string()),
        "the contact account is read fresh too"
    );
}

/// The endpoint is merged under `/api/v1`, and the trailing-slash
/// spelling is no second mount: it takes the app-wide API fallback.
#[sqlx::test(migrator = "wired_be::MIGRATOR")]
async fn the_route_lives_at_api_v1_instance(pool: PgPool) {
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/api/v1/instance/")
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "the slash spelling is not a second mount"
    );
    assert_eq!(
        response.json_value(),
        json!({"error": "Not implemented"}),
        "Pleroma's api_not_implemented fallback"
    );
    let _document = get_instance(&app).await;
}
