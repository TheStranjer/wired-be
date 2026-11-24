//! Tests for `GET /.well-known/nodeinfo` and `GET /nodeinfo/{version}`
//! (`src/api/nodeinfo.rs`).
//!
//! The discovery document links the two schema documents at their
//! `.json`-suffixed addresses; each schema document reports the software
//! identity, live usage counters and Pleroma metadata, with 2.1 adding the
//! software repository links. Both spellings of a version — `2.0` and
//! `2.0.json` — serve the same document with the same `profile`
//! content-type; any other version name is Pleroma's 404.
//!
//! The path parsing and document building are covered where they live, in
//! `src/api/nodeinfo.rs`; what these tests own is the route, the database
//! reads, the exact key sets and values, and the content-type parameter.

#![cfg(test)]

use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use wired_be::{
    config::Toggle,
    models::{
        status::{self, NewStatus, Visibility},
        user::NewUser,
    },
    services::instance_meta,
};

use crate::common;

/// The instance origin of [`common::test_config`].
const BASE: &str = "https://test.example";
/// The `NodeInfo` 2.0 schema URI.
const SCHEMA_20: &str = "http://nodeinfo.diaspora.software/ns/schema/2.0";
/// The `NodeInfo` 2.1 schema URI.
const SCHEMA_21: &str = "http://nodeinfo.diaspora.software/ns/schema/2.1";
/// The repository the 2.1 document advertises.
const REPOSITORY: &str = "https://github.com/TheStranjer/wired-be";

/// The software version a client should see: this crate's own version,
/// naming the Pleroma release this interface is compatible with.
fn software_version() -> String {
    format!("{} (compatible; Pleroma 2.10.2)", env!("CARGO_PKG_VERSION"))
}

/// The top-level keys every `NodeInfo` document carries, as a parsed
/// [`Value`] orders them.
const TOP_KEYS: [&str; 7] = [
    "metadata",
    "openRegistrations",
    "protocols",
    "services",
    "software",
    "usage",
    "version",
];

/// The `metadata` keys both versions carry, in parsed order — the ticket's
/// key set, with Pleroma's `chat` block left out as it prescribes.
const METADATA_KEYS: [&str; 17] = [
    "accountActivationRequired",
    "features",
    "federation",
    "fieldsLimits",
    "invitesEnabled",
    "mailerEnabled",
    "markup",
    "nodeDescription",
    "nodeName",
    "pollLimits",
    "postFormats",
    "private",
    "restrict_unauthenticated",
    "skipThreadContainment",
    "staffAccounts",
    "suggestions",
    "uploadLimits",
];

/// The feature list Pleroma 2.10.2 advertises with its shipped defaults,
/// spelled out here so the shared builder is checked, not echoed.
const EXPECTED_FEATURES: [&str; 25] = [
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
];

/// The sorted object keys of `value`, for key-set assertions.
fn keys_of(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect()
}

/// The complete `NodeInfo` document an empty instance built on
/// [`common::test_config`] serves for `version` (`"2.0"` or `"2.1"`):
/// zeroed usage, no staff, and the Pleroma-default metadata.
fn expected_document(version: &str) -> Value {
    let mut document = json!({
        "version": "2.0",
        "software": { "name": "wired", "version": software_version() },
        "protocols": ["activitypub"],
        "services": { "inbound": [], "outbound": [] },
        "openRegistrations": true,
        "usage": {
            "users": { "total": 0, "activeMonth": 0, "activeHalfyear": 0 },
            "localPosts": 0,
        },
        "metadata": {
            "nodeName": "Wired",
            "nodeDescription": "A Wired instance, compatible with Pleroma clients",
            "private": false,
            "suggestions": { "enabled": false },
            "staffAccounts": [],
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
            "pollLimits": {
                "max_options": 20,
                "max_option_chars": 200,
                "min_expiration": 0,
                "max_expiration": 31_536_000,
            },
            "postFormats": ["text/plain", "text/html", "text/markdown"],
            "uploadLimits": {
                "general": 16_000_000,
                "avatar": 2_000_000,
                "banner": 4_000_000,
                "background": 4_000_000,
            },
            "fieldsLimits": {
                "maxFields": 10,
                "maxRemoteFields": 20,
                "nameLength": 512,
                "valueLength": 2048,
            },
            "accountActivationRequired": false,
            "invitesEnabled": false,
            "mailerEnabled": false,
            "features": EXPECTED_FEATURES,
            "restrict_unauthenticated": {
                "timelines": { "local": false, "federated": false },
                "profiles": { "local": false, "remote": false },
                "activities": { "local": false, "remote": false },
            },
            "skipThreadContainment": true,
            "markup": {
                "allow_inline_images": true,
                "allow_headings": true,
                "allow_tables": false,
                "allow_fonts": false,
            },
        },
    });
    if version == "2.1" {
        document["version"] = json!("2.1");
        document["software"]["repository"] = json!(REPOSITORY);
        document["software"]["homepage"] = json!(REPOSITORY);
    }
    document
}

/// Fetch `path` and require a 200 JSON document.
async fn get_document(app: &axum::Router, path: &str) -> Value {
    let response = common::TestRequest::get(path).send(app).await;
    assert_eq!(response.status, StatusCode::OK, "{path} is served");
    response.json_value()
}

/// `GET /.well-known/nodeinfo` links both schema documents at their
/// `.json`-suffixed addresses under `BASE_URL`, and both hrefs resolve.
#[tokio::test]
async fn discovery_links_both_schema_documents() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/.well-known/nodeinfo")
        .send(&app)
        .await;
    assert_eq!(
        response.status,
        StatusCode::OK,
        "the discovery document is public"
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "discovery is plain JSON without a profile"
    );
    let links = response.json_value();
    assert_eq!(
        links,
        json!({
            "links": [
                { "rel": SCHEMA_20, "href": format!("{BASE}/nodeinfo/2.0.json") },
                { "rel": SCHEMA_21, "href": format!("{BASE}/nodeinfo/2.1.json") },
            ],
        }),
        "the two links, in Pleroma's order"
    );
    for href in [
        "https://test.example/nodeinfo/2.0.json",
        "https://test.example/nodeinfo/2.1.json",
    ] {
        let _linked = get_document(&app, href).await;
    }
}

/// The 2.0 document, key by key: the ticket's key sets and Pleroma 2.10.2's
/// default metadata on a fresh instance.
#[tokio::test]
async fn nodeinfo_20_is_the_full_pleroma_document() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let document = get_document(&app, "/nodeinfo/2.0").await;
    assert_eq!(document, expected_document("2.0"), "the whole document");
    assert_eq!(
        keys_of(&document),
        TOP_KEYS,
        "the ticket's top-level key set"
    );
    assert_eq!(
        keys_of(&document["metadata"]),
        METADATA_KEYS,
        "the ticket's metadata key set"
    );
    assert_eq!(
        document["metadata"]["chat"],
        Value::Null,
        "the optional chat block stays omitted"
    );
}

/// The 2.1 document repeats 2.0's key sets under its own `version`, with
/// the software links the 2.0 schema has no keys for.
#[tokio::test]
async fn nodeinfo_21_adds_the_software_links() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let bare = get_document(&app, "/nodeinfo/2.1").await;
    let suffixed = get_document(&app, "/nodeinfo/2.1.json").await;
    assert_eq!(bare, suffixed, "both spellings are one document");
    assert_eq!(bare, expected_document("2.1"), "the whole document");
    assert_eq!(bare["version"], "2.1", "the 2.1 schema declares itself");
    assert_eq!(
        keys_of(&bare["software"]),
        vec!["homepage", "name", "repository", "version"],
        "2.1 software carries the links"
    );
    assert_eq!(
        keys_of(&bare["metadata"]),
        keys_of(&suffixed["metadata"]),
        "one metadata key set across the mount points"
    );
    let two_oh = get_document(&app, "/nodeinfo/2.0").await;
    assert_eq!(
        keys_of(&two_oh["metadata"]),
        keys_of(&bare["metadata"]),
        "2.0 and 2.1 share the metadata key set"
    );
}

/// Every document path — four mounts, two schemas — answers with the
/// schema's URI as the `profile` content-type parameter.
#[tokio::test]
async fn each_document_carries_its_schema_profile() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    for (path, schema) in [
        ("/nodeinfo/2.0", SCHEMA_20),
        ("/nodeinfo/2.0.json", SCHEMA_20),
        ("/nodeinfo/2.1", SCHEMA_21),
        ("/nodeinfo/2.1.json", SCHEMA_21),
    ] {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(response.status, StatusCode::OK, "{path} is served");
        let expected = format!("application/json; profile=\"{schema}#\"");
        assert_eq!(
            response.header("content-type"),
            Some(expected.as_str()),
            "{path} profiles itself as {schema}"
        );
    }
}

/// A version this server does not document is Pleroma's 404; the bare
/// `/nodeinfo` path is no document at all and takes the router fallback.
#[tokio::test]
async fn unserved_versions_are_404() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    for path in [
        "/nodeinfo/2.2",
        "/nodeinfo/1.0",
        "/nodeinfo/latest",
        "/nodeinfo/2.0.htm",
    ] {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{path} is a 404");
        assert_eq!(
            response.json_value(),
            json!({"error": "Nodeinfo schema version not handled"}),
            "{path} carries Pleroma's message"
        );
    }
    let response = common::TestRequest::get("/nodeinfo").send(&app).await;
    assert_eq!(
        response.status,
        StatusCode::NOT_FOUND,
        "bare /nodeinfo 404s"
    );
    assert_eq!(
        response.json_value(),
        json!({"error": "Not found"}),
        "the router fallback, not a schema error"
    );
}

/// A moment `days` days before now, for the activity-window fixtures.
fn days_ago(days: i64) -> DateTime<Utc> {
    Utc::now() - Duration::days(days)
}

/// The usage counters count local active users, the 30- and 180-day
/// activity windows (`last_active_at`, falling back to the newest post),
/// and live non-reblog posts of this instance only.
#[tokio::test]
async fn usage_counts_local_users_and_posts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user_with(&pool, "alice", |user| NewUser {
        last_active_at: Some(days_ago(1)),
        ..user
    })
    .await;
    // A post from day 10: counts in the month through the fallback.
    let carol = common::create_local_user_with(&pool, "carol", |user| NewUser {
        last_status_at: Some(days_ago(10)),
        ..user
    })
    .await;
    // Last posted on day 100: the half-year window only.
    let _dan = common::create_local_user_with(&pool, "dan", |user| NewUser {
        last_status_at: Some(days_ago(100)),
        ..user
    })
    .await;
    // A local-only post still counts as activity, through `last_active_at`.
    let eve = common::create_local_user_with(&pool, "eve", |user| NewUser {
        last_active_at: Some(days_ago(60)),
        ..user
    })
    .await;
    // Never seen (counts in the total only), deactivated, and remote: the
    // last two count nowhere.
    let _bob = common::create_local_user(&pool, "bob").await;
    let _erin = common::create_local_user_with(&pool, "erin", |user| NewUser {
        is_active: false.into(),
        last_active_at: Some(days_ago(1)),
        ..user
    })
    .await;
    let fern = common::create_remote_user(&pool, "fern", "remote.example").await;

    // Three live local posts, one tombstone, one reblog and one remote
    // post: only the three count. Dan has no post: the `localPosts`
    // counter must not see his row either way.
    let _one = common::create_status(&pool, &alice, "one", Visibility::Public).await;
    let doomed = common::create_status(&pool, &alice, "gone", Visibility::Public).await;
    let original = common::create_status(&pool, &carol, "hers", Visibility::Public).await;
    let _local_only = common::create_status(&pool, &eve, "local only", Visibility::Local).await;
    let _tombstone = status::soft_delete(&pool, doomed.id)
        .await
        .expect("the tombstone write runs");
    let reblog = NewStatus {
        reblog_of_id: Some(original.id),
        ..NewStatus::local(&alice, "")
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
    for path in ["/nodeinfo/2.0", "/nodeinfo/2.1.json"] {
        let document = get_document(&app, path).await;
        assert_eq!(
            document["usage"],
            json!({
                "users": { "total": 5, "activeMonth": 2, "activeHalfyear": 4 },
                "localPosts": 3,
            }),
            "{path} carries the same live counters"
        );
    }
}

/// The usage block is served from the same cached counters as
/// `/api/v1/instance`: a second call inside the window does not see a new
/// account or post, and `localPosts`/`users.total` equal that document's
/// `status_count`/`user_count`. The window's expiry is tested with an
/// injected clock in `src/api/instance/stats.rs`.
#[tokio::test]
async fn usage_is_the_cached_instance_stats() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let _post = common::create_status(&pool, &alice, "first", Visibility::Public).await;
    let app = common::test_app(pool.clone());

    let first = get_document(&app, "/nodeinfo/2.0").await;
    assert_eq!(
        first["usage"]["users"]["total"], 1,
        "the first read is live"
    );
    assert_eq!(first["usage"]["localPosts"], 1, "and counts the post");

    let _bob = common::create_local_user(&pool, "bob").await;
    let _late = common::create_status(&pool, &alice, "late", Visibility::Public).await;

    let second = get_document(&app, "/nodeinfo/2.0").await;
    assert_eq!(
        second["usage"], first["usage"],
        "the open window does not see the new account or post"
    );

    let instance = common::TestRequest::get("/api/v1/instance")
        .send(&app)
        .await
        .json_value();
    assert_eq!(
        instance["stats"]["user_count"], first["usage"]["users"]["total"],
        "one source for the user count"
    );
    assert_eq!(
        instance["stats"]["status_count"], first["usage"]["localPosts"],
        "one source for the post count"
    );
}

/// The staff list holds the actor ids of the local admin and moderator
/// accounts — and only theirs, ordered by id.
#[tokio::test]
async fn staff_accounts_lists_admins_and_moderators() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let _zed = common::create_local_user_with(&pool, "zed", |user| NewUser {
        is_moderator: true.into(),
        ..user
    })
    .await;
    let _alice = common::create_local_user_with(&pool, "alice", |user| NewUser {
        is_admin: true.into(),
        ..user
    })
    .await;
    let _carol = common::create_local_user(&pool, "carol").await;
    // A deactivated moderator and a remote admin are not staff to report.
    let _dave = common::create_local_user_with(&pool, "dave", |user| NewUser {
        is_moderator: true.into(),
        is_active: false.into(),
        ..user
    })
    .await;
    let _eve = common::create_local_user_with(&pool, "eve", |user| NewUser {
        local: false.into(),
        is_admin: true.into(),
        ..user
    })
    .await;

    let app = common::test_app(pool);
    let document = get_document(&app, "/nodeinfo/2.0").await;
    assert_eq!(
        document["metadata"]["staffAccounts"],
        json!([
            "https://test.example/users/alice",
            "https://test.example/users/zed",
        ]),
        "the live local staff, ordered by actor id"
    );
}

/// `openRegistrations` follows `REGISTRATIONS_OPEN`, `invitesEnabled`
/// follows `INVITES_ENABLED`, and the feature and access-restriction
/// blocks are the ones the shared instance-metadata builders produce.
#[tokio::test]
async fn registrations_toggle_and_shared_metadata_follow_the_builders() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = common::test_config();
    config.registrations_open = Toggle::new(false);
    config.invites_enabled = Toggle::new(true);
    let app = common::test_app_with_config(pool, config);
    let document = get_document(&app, "/nodeinfo/2.0").await;
    assert_eq!(
        document["openRegistrations"],
        json!(false),
        "a closed instance reports itself closed"
    );
    assert_eq!(
        document["metadata"]["invitesEnabled"],
        json!(true),
        "INVITES_ENABLED=true is reported"
    );
    assert_eq!(
        document["metadata"]["features"],
        json!(instance_meta::features()),
        "the shared feature list"
    );
    assert_eq!(
        document["metadata"]["restrict_unauthenticated"],
        instance_meta::restrict_unauthenticated(&common::test_config()),
        "the shared access restrictions"
    );
}

/// `RESTRICT_UNAUTHENTICATED` is what `/nodeinfo/2.1` advertises: exactly
/// the configured areas are `true`.
#[tokio::test]
async fn restricted_areas_are_advertised_in_the_metadata() {
    let db = common::test_db().await;
    let config =
        common::test_config_with_restricted_areas(&["timelines_federated", "activities_local"]);
    let app = common::test_app_with_config(db.pool().clone(), config);
    let document = get_document(&app, "/nodeinfo/2.1").await;
    assert_eq!(
        document["metadata"]["restrict_unauthenticated"],
        json!({
            "timelines": { "local": false, "federated": true },
            "profiles": { "local": false, "remote": false },
            "activities": { "local": true, "remote": false },
        })
    );
}

/// `metadata.federation` reports the live MRF chain — the instance
/// setting's policy names in Pleroma formatting beside each policy's
/// `mrf_<policy>` transparency object — the same answer the instance
/// endpoints give.
#[tokio::test]
async fn federation_reports_the_enabled_mrf_policies() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    wired_be::services::runtime_config::write(
        &pool,
        ":pleroma",
        ":mrf",
        &json!({"policies": ["noop"]}),
    )
    .await
    .expect("the mrf entry writes");
    let app = common::test_app(pool);
    let document = get_document(&app, "/nodeinfo/2.0").await;
    let federation = &document["metadata"]["federation"];
    assert_eq!(
        federation["mrf_policies"],
        json!(["NoOpPolicy", "HashtagPolicy"]),
        "the chain's names in Pleroma formatting"
    );
    assert_eq!(
        federation["mrf_noop"],
        json!({}),
        "the shipped policy describes itself empty"
    );
    assert_eq!(federation["enabled"], true, "still federating");
}

/// `metadata.federation` is the very answer the instance documents publish
/// — the policy names beside their `mrf_simple` and `mrf_simple_info`
/// objects, the exclusions marker, and with `MRF_TRANSPARENCY` off nothing
/// but the three fixed keys.
#[tokio::test]
async fn federation_is_the_same_answer_the_instance_documents_give() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    wired_be::services::runtime_config::write(
        &pool,
        ":pleroma",
        ":mrf",
        &json!({
            "policies": ["simple"],
            "simple": {
                "reject": [["evil.example", "bots"]],
                "followers_only": ["press.example"],
            },
        }),
    )
    .await
    .expect("the mrf entry writes");
    for transparency in [true, false] {
        let mut config = common::test_config();
        config.mrf.transparency = Toggle::new(transparency);
        config.mrf.transparency_exclusions = vec!["press.example".to_owned()];
        let app = common::test_app_with_config(pool.clone(), config);
        let nodeinfo = get_document(&app, "/nodeinfo/2.0").await;
        let instance = common::TestRequest::get("/api/v1/instance")
            .send(&app)
            .await
            .json_value();
        let federation = nodeinfo["metadata"]["federation"].clone();
        assert_eq!(
            federation, instance["pleroma"]["metadata"]["federation"],
            "MRF_TRANSPARENCY={transparency}: NodeInfo and the instance \
             documents publish one answer"
        );
        if transparency {
            assert_eq!(
                federation["mrf_policies"],
                json!(["SimplePolicy", "HashtagPolicy"]),
                "the running policy is named"
            );
            assert_eq!(
                federation["mrf_simple"]["reject"],
                json!(["evil.example"]),
                "and its lists are published"
            );
            assert_eq!(
                federation["mrf_simple"]["followers_only"],
                json!([]),
                "the excluded host is missing from the list it was on"
            );
            assert_eq!(
                federation["exclusions"], true,
                "and the exclusion is disclosed"
            );
        } else {
            assert_eq!(
                federation,
                json!({"enabled": true, "mrf_policies": [], "exclusions": false}),
                "with transparency off the block is the three fixed keys"
            );
        }
    }
}
