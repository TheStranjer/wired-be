//! The instance sub-resources over HTTP: rules, domain blocks, translation
//! languages, activity, the extended description and their v1-only routing.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::{config::Toggle, models::instance_rule};

use super::support::{SIMPLE_LISTS, keys_of, other_config, set_mrf};
use crate::common;

/// `GET /api/v1/instance/rules` answers the same entries, in the same
/// priority-then-id order, as the v1 document's `rules` block — and the
/// empty array while no rule is written. No authentication.
#[tokio::test]
async fn the_rules_endpoint_serves_the_instance_rules() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let empty = common::TestRequest::get("/api/v1/instance/rules")
        .send(&app)
        .await;
    assert_eq!(empty.status, StatusCode::OK, "the listing is served");
    assert_eq!(empty.json_value(), json!([]), "no rules written yet");

    let first = instance_rule::create(&pool, "Be nice", "", 0)
        .await
        .expect("the first rule inserts");
    let second = instance_rule::create(&pool, "No spam", "we delete it", 5)
        .await
        .expect("the second rule inserts");

    let response = common::TestRequest::get("/api/v1/instance/rules")
        .bearer("not-a-real-token")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "credentials are ignored");
    assert_eq!(
        response.json_value(),
        json!([
            {"id": first.id.to_string(), "text": "Be nice", "hint": ""},
            {"id": second.id.to_string(), "text": "No spam", "hint": "we delete it"},
        ]),
        "id, text and hint, priority-ordered like the document block"
    );
}

/// `GET /api/v1/instance/domain_blocks` renders the
/// [`simple`](wired_be::federation::mrf::simple) policy's domain lists as
/// Mastodon domain blocks: `reject` suspends, the four lists that keep a
/// host's posts out of the public eye all silence, and every entry carries
/// the lowercase-hex `SHA-256` of the domain it prints beside the
/// operator's reason (empty when ungiven) — patterns verbatim, wildcards
/// included. An instance that runs no `simple` policy answers the empty
/// array. No authentication.
#[tokio::test]
async fn the_domain_blocks_endpoint_maps_the_simple_lists_to_severities() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let empty = common::TestRequest::get("/api/v1/instance/domain_blocks")
        .send(&app)
        .await;
    assert_eq!(empty.status, StatusCode::OK, "the listing is served");
    assert_eq!(
        empty.json_value(),
        json!([]),
        "no policy configured yet, and no `simple` policy means no blocks"
    );

    let setting: Value = serde_json::from_str(SIMPLE_LISTS).expect("the fixture is JSON");
    set_mrf(&pool, &setting).await;

    // A fresh app: the one above cached the empty configuration it read.
    let configured = common::test_app(pool);
    let response = common::TestRequest::get("/api/v1/instance/domain_blocks")
        .bearer("not-a-real-token")
        .send(&configured)
        .await;
    assert_eq!(response.status, StatusCode::OK, "credentials are ignored");
    assert_eq!(
        response.json_value(),
        json!([
            {
                "domain": "blocked.example",
                "digest": wired_be::api::instance::sha256_hex("blocked.example"),
                "severity": "suspend",
                "comment": "coordinated abuse",
            },
            {
                "domain": "*.worse.example",
                "digest": wired_be::api::instance::sha256_hex("*.worse.example"),
                "severity": "suspend",
                "comment": "",
            },
            {
                "domain": "noisy.example",
                "digest": wired_be::api::instance::sha256_hex("noisy.example"),
                "severity": "silence",
                "comment": "",
            },
            {
                "domain": "press.example",
                "digest": wired_be::api::instance::sha256_hex("press.example"),
                "severity": "silence",
                "comment": "",
            },
            {
                "domain": "leech.example",
                "digest": wired_be::api::instance::sha256_hex("leech.example"),
                "severity": "silence",
                "comment": "",
            },
            {
                "domain": "lewd.example",
                "digest": wired_be::api::instance::sha256_hex("lewd.example"),
                "severity": "silence",
                "comment": "no nsfw",
            },
        ]),
        "the rejected hosts first, then the softened lists in configuration \
         order; the avatar strip is no domain block"
    );
}

/// With `MRF_TRANSPARENCY` off the domain-block listing publishes nothing,
/// and an excluded host is missing from it while the rest of its list
/// stands: the listing is the transparency answer, not a second source of
/// moderation facts.
#[tokio::test]
async fn domain_blocks_follow_the_transparency_settings() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let setting: Value = serde_json::from_str(SIMPLE_LISTS).expect("the fixture is JSON");
    set_mrf(&pool, &setting).await;

    let mut off = common::test_config();
    off.mrf.transparency = Toggle::new(false);
    let shut = common::test_app_with_config(pool.clone(), off);
    let hidden = common::TestRequest::get("/api/v1/instance/domain_blocks")
        .send(&shut)
        .await;
    assert_eq!(
        hidden.json_value(),
        json!([]),
        "an instance that describes no filter lists no blocked domain, while \
         the policy stays in force"
    );

    let mut excluding = common::test_config();
    excluding.mrf.transparency_exclusions = vec!["press.example".to_owned()];
    let partial = common::test_app_with_config(pool.clone(), excluding);
    let blocks = common::TestRequest::get("/api/v1/instance/domain_blocks")
        .send(&partial)
        .await
        .json_value();
    let listed = blocks
        .as_array()
        .expect("the listing is an array")
        .iter()
        .map(|entry| entry["domain"].as_str().unwrap_or_default())
        .collect::<Vec<&str>>();
    assert_eq!(
        listed,
        vec![
            "blocked.example",
            "*.worse.example",
            "noisy.example",
            "leech.example",
            "lewd.example",
        ],
        "the excluded host is simply not there, its list's neighbours are"
    );
}

/// `GET /api/v1/instance/translation_languages` answers the empty
/// object of [`common::test_config`], which configures no translation
/// provider — the document Pleroma's `translation_languages.json` renders
/// then. A configured provider's matrix, and its cache, are
/// `translate::the_language_matrix_is_served_and_cached`. No authentication.
#[tokio::test]
async fn translation_languages_is_the_empty_object() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/api/v1/instance/translation_languages")
        .bearer("not-a-real-token")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the document is served");
    assert_eq!(
        response.json_value(),
        json!({}),
        "an unconfigured provider has no language matrix"
    );
}

/// `GET /api/v1/instance/activity` answers the empty array: wired-be
/// keeps no weekly activity counters. No authentication.
#[tokio::test]
async fn activity_is_the_empty_array() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    let response = common::TestRequest::get("/api/v1/instance/activity")
        .bearer("not-a-real-token")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the listing is served");
    assert_eq!(
        response.json_value(),
        json!([]),
        "no counted week exists yet"
    );
}

/// `GET /api/v1/instance/extended_description` wraps the configured
/// description in a paragraph and stamps it with the current time in
/// Mastodon's format. No authentication.
#[tokio::test]
async fn the_extended_description_wraps_the_instance_description() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());
    let response = common::TestRequest::get("/api/v1/instance/extended_description")
        .bearer("not-a-real-token")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the document is served");
    let document = response.json_value();
    assert_eq!(
        keys_of(&document),
        vec!["content", "updated_at"],
        "the ticket's two keys"
    );
    assert_eq!(
        document["content"], "<p>A Wired instance, compatible with Pleroma clients</p>",
        "the INSTANCE_DESCRIPTION paragraph"
    );
    let stamp = document["updated_at"]
        .as_str()
        .expect("the update time is a string");
    assert_eq!(stamp.len(), 24, "exactly the Mastodon timestamp shape");
    let when = wired_be::util::time::parse_time(stamp).expect("a parseable stamp");
    let age = chrono::Utc::now().signed_duration_since(when).num_seconds();
    assert!((-5..=60).contains(&age), "the stamp is the request time");

    let other = common::test_app_with_config(pool, other_config());
    let rewritten = common::TestRequest::get("/api/v1/instance/extended_description")
        .send(&other)
        .await
        .json_value();
    assert_eq!(
        rewritten["content"], "<p>Where we all chatter</p>",
        "the content follows INSTANCE_DESCRIPTION"
    );
}

/// The `/instance` sub-resources are mounted under `/api/v1` only —
/// Pleroma's router offers no v2 spelling — while `/api/v2/instance`
/// itself answers: the v2 mount does not shadow or extend the v1 list.
#[tokio::test]
async fn the_sub_resources_are_v1_only() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool);
    for path in [
        "/api/v2/instance/peers",
        "/api/v2/instance/rules",
        "/api/v2/instance/domain_blocks",
    ] {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{path} is not a second mount"
        );
        assert_eq!(
            response.json_value(),
            json!({"error": "Not implemented"}),
            "Pleroma's api_not_implemented fallback"
        );
    }
    let document = common::TestRequest::get("/api/v2/instance")
        .send(&app)
        .await;
    assert_eq!(
        document.status,
        StatusCode::OK,
        "the v2 document itself stays served"
    );
}
