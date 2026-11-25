//! The v2 instance document and the peers listing over HTTP.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::{
    entities::account::Account,
    models::{instance_rule, user::NewUser},
    services::instance_meta,
};

use super::support::{
    BASE, expected_configuration, expected_pleroma, instance_vapid_key, instance_version, keys_of,
    mirror_remote, other_config,
};
use crate::common;

/// The sorted top-level keys of the v2 document — the ticket's key set.
const V2_TOP_KEYS: [&str; 14] = [
    "configuration",
    "contact",
    "description",
    "domain",
    "languages",
    "pleroma",
    "registrations",
    "rules",
    "source_url",
    "thumbnail",
    "title",
    "usage",
    "version",
    "wired",
];

/// The `configuration` block of the empty [`common::test_config`] v2
/// document: the three groups v1 shares, spelled via
/// [`expected_configuration`] to prove they are identical, beside
/// Mastodon 4's own groups.
fn expected_v2_configuration(vapid_key: &str) -> Value {
    let v1 = expected_configuration();
    json!({
        "urls": { "streaming": "wss://test.example", "status": null },
        "vapid": { "public_key": vapid_key },
        "accounts": { "max_featured_tags": 10, "max_pinned_statuses": 5 },
        "statuses": v1["statuses"].clone(),
        "media_attachments": v1["media_attachments"].clone(),
        "polls": v1["polls"].clone(),
        // `common::test_config` names no translation provider.
        "translation": { "enabled": false },
    })
}

/// The complete v2 document an empty instance built on
/// [`common::test_config`] serves.
fn expected_v2_document(vapid_key: &str) -> Value {
    json!({
        "domain": "test.example",
        "title": "Wired",
        "version": instance_version(),
        "source_url": "https://github.com/TheStranjer/wired-be",
        "description": "A Wired instance, compatible with Pleroma clients",
        "usage": { "users": { "active_month": 0 } },
        "thumbnail": { "url": format!("{BASE}/media/default/thumbnail.jpg") },
        "languages": ["en"],
        "configuration": expected_v2_configuration(vapid_key),
        "registrations": {
            "enabled": true,
            "approval_required": false,
            "message": null,
            "url": null,
        },
        "contact": { "email": "", "account": null },
        "rules": [],
        "pleroma": expected_pleroma(vapid_key),
        "wired": { "protocols": ["activitypub"] },
    })
}

/// `GET /api/v2/instance` answers the ticket's reshaped document — the
/// v1 `media_attachments`, `polls` and `pleroma` blocks reused whole —
/// without consulting credentials; a wrong method is the plain 405.
#[tokio::test]
async fn the_v2_document_is_public_and_complete() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let vapid_key = instance_vapid_key(&pool).await;
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/api/v2/instance")
        .bearer("not-a-real-token")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the v2 document is served");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "the document is JSON"
    );
    let document = response.json_value();
    assert_eq!(keys_of(&document), V2_TOP_KEYS, "the ticket's v2 key set");
    assert_eq!(
        document,
        expected_v2_document(&vapid_key),
        "the whole document"
    );
    assert_eq!(
        document["pleroma"]["metadata"]["features"],
        json!(instance_meta::features()),
        "the pleroma block is the shared builder, not a restatement"
    );
    let posted = common::TestRequest::post("/api/v2/instance")
        .send(&app)
        .await;
    assert_eq!(
        posted.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "the endpoint is read-only"
    );
}

/// The v2 document mirrors the config (domain, streaming scheme,
/// thumbnail, limits, registration state, contact email) and the
/// fixtures (rules in display order, the first local admin as the full
/// `Account` entity nested in `contact`).
#[tokio::test]
async fn the_v2_document_follows_the_config_and_fixtures() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let rule = instance_rule::create(&pool, "Be nice", "say hi", 0)
        .await
        .expect("the rule inserts");
    let root = common::create_local_user_with(&pool, "root", |user| NewUser {
        is_admin: true.into(),
        ..user
    })
    .await;
    let config = other_config();
    let app = common::test_app_with_config(pool, config.clone());
    let document = common::TestRequest::get("/api/v2/instance")
        .send(&app)
        .await
        .json_value();
    assert_eq!(document["domain"], "snatter.example:4000", "the domain");
    assert_eq!(document["title"], "Snatter", "INSTANCE_NAME");
    assert_eq!(
        document["description"], "Where we all chatter",
        "INSTANCE_DESCRIPTION"
    );
    assert_eq!(
        document["source_url"], "https://github.com/TheStranjer/wired-be",
        "the shipped repository URL"
    );
    assert_eq!(
        document["thumbnail"]["url"], "http://snatter.example:4000/media/default/thumbnail.jpg",
        "the wrapped thumbnail follows MEDIA_BASE_URL"
    );
    assert_eq!(
        document["configuration"]["urls"]["streaming"], "ws://snatter.example:4000",
        "an http origin gets the plain ws scheme"
    );
    assert_eq!(
        document["configuration"]["statuses"],
        json!({"max_characters": 300, "max_media_attachments": 6, "characters_reserved_per_url": 23}),
        "MAX_STATUS_CHARS and MAX_MEDIA_ATTACHMENTS mirror the config"
    );
    assert_eq!(
        document["configuration"]["polls"]["max_options"], 12,
        "MAX_POLL_OPTIONS mirrors the config"
    );
    assert_eq!(
        document["registrations"]["enabled"], false,
        "REGISTRATIONS_OPEN=false is advertised"
    );
    assert_eq!(
        document["contact"]["email"], "admin@example.org",
        "CONTACT_EMAIL"
    );
    assert_eq!(
        document["contact"]["account"],
        serde_json::to_value(Account::from_user(&root, &config)).unwrap(),
        "the first local admin renders as the full Account entity"
    );
    assert_eq!(
        document["rules"],
        json!([{"id": rule.id.to_string(), "text": "Be nice", "hint": "say hi"}]),
        "the rules block matches the v1 rendering"
    );
}

/// `GET /api/v1/instance/peers` answers the distinct remote hosts,
/// alphabetically, local users (whose `domain` is null) never among
/// them; a garbage bearer is not consulted, and an instance mirroring
/// nobody answers the empty array.
#[tokio::test]
async fn peers_lists_distinct_remote_domains() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let empty = common::TestRequest::get("/api/v1/instance/peers")
        .send(&app)
        .await;
    assert_eq!(empty.status, StatusCode::OK, "peers are served");
    assert_eq!(
        empty.json_value(),
        json!([]),
        "an instance that mirrors nobody lists no peers"
    );

    let _alice = common::create_local_user(&pool, "alice").await;
    mirror_remote(&pool, "remote.example").await;
    let _gap = common::create_remote_user(&pool, "gap", "remote.example").await;
    mirror_remote(&pool, "other.example").await;

    let response = common::TestRequest::get("/api/v1/instance/peers")
        .bearer("not-a-real-token")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "credentials are ignored");
    assert_eq!(
        response.json_value(),
        json!(["other.example", "remote.example"]),
        "each host once, sorted, locals excluded"
    );

    let posted = common::TestRequest::post("/api/v1/instance/peers")
        .send(&app)
        .await;
    assert_eq!(
        posted.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "the endpoint is read-only"
    );
}

/// The listing is capped at the thousand hosts the endpoint documents,
/// keeping the alphabetically first thousand of a larger table.
#[tokio::test]
async fn peers_are_capped_at_one_thousand() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    // 1001 distinct hosts in one set-based write; the ids are test
    // fixtures, so the database-generated ones are fine here.
    let inserted = sqlx::query(
        "INSERT INTO users (id, nickname, ap_id, local, domain)
         SELECT gen_random_uuid(),
                'peeruser' || g || '@peer' || g || '.example',
                'https://peer' || g || '.example/users/peeruser' || g,
                false,
                'peer' || g || '.example'
           FROM generate_series(1, 1001) AS g",
    )
    .execute(&pool)
    .await
    .expect("the peer fixtures insert");
    assert_eq!(inserted.rows_affected(), 1001, "the table holds 1001 hosts");

    let app = common::test_app(pool);
    let peers: Vec<String> = common::TestRequest::get("/api/v1/instance/peers")
        .send(&app)
        .await
        .json();
    assert_eq!(peers.len(), 1000, "the listing stops at the documented cap");
    let mut sorted = peers.clone();
    sorted.sort();
    assert_eq!(sorted, peers, "the answer stays alphabetical");
    assert!(
        !peers.iter().any(|domain| domain == "peer999.example"),
        "the alphabetically last host is the one cut off"
    );
    assert!(
        peers.iter().any(|domain| domain == "peer1.example"),
        "the listing starts at the top"
    );
}

/// With `timelines_federated` restricted, the peer list is hidden from
/// everyone — it is the anonymous-readable endpoint — and the instance
/// documents advertise exactly the configured areas.
#[tokio::test]
async fn restricted_areas_hide_the_peers_and_are_advertised() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    mirror_remote(&pool, "remote.example").await;
    let open = common::test_app(pool.clone());
    let listed = common::TestRequest::get("/api/v1/instance/peers")
        .send(&open)
        .await;
    assert_eq!(
        listed.json_value(),
        json!(["remote.example"]),
        "the default config lists peers"
    );

    let app =
        common::test_app_with_restricted_areas(pool, &["timelines_federated", "activities_local"]);
    let hidden = common::TestRequest::get("/api/v1/instance/peers")
        .send(&app)
        .await;
    assert_eq!(hidden.status, StatusCode::OK, "the endpoint still answers");
    assert_eq!(hidden.json_value(), json!([]), "no peers are listed");

    let expected = json!({
        "timelines": { "local": false, "federated": true },
        "profiles": { "local": false, "remote": false },
        "activities": { "local": true, "remote": false },
    });
    for path in ["/api/v1/instance", "/api/v2/instance"] {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(response.status, StatusCode::OK, "{path} is served");
        assert_eq!(
            response.json_value()["pleroma"]["metadata"]["restrict_unauthenticated"],
            expected,
            "{path} advertises the configured restrictions"
        );
    }

    let other = common::test_app_with_restricted_areas(db.pool().clone(), &["profiles_local"]);
    let still_listed = common::TestRequest::get("/api/v1/instance/peers")
        .send(&other)
        .await;
    assert_eq!(
        still_listed.json_value(),
        json!(["remote.example"]),
        "other restricted areas leave the peer list alone"
    );
}
