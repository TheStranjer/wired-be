//! The `pleroma.metadata.federation` block over HTTP: the enabled MRF
//! chain, the published simple lists, transparency and peer exclusion.

use axum::http::StatusCode;
use serde_json::{Value, json};
use wired_be::config::Toggle;

use super::support::{SIMPLE_LISTS, mirror_remote, set_mrf};
use crate::common;

/// The `pleroma.metadata.federation` block reports the live MRF chain:
/// the `mrf` instance setting's policy names in Pleroma formatting
/// beside each policy's sanitized `mrf_<policy>` transparency object, on
/// both document versions. Unknown names drop out of the chain, not into
/// the document.
#[tokio::test]
async fn federation_reports_the_enabled_mrf_policies() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    wired_be::services::runtime_config::write(
        &pool,
        ":pleroma",
        ":mrf",
        &json!({"policies": ["ghost", "noop"]}),
    )
    .await
    .expect("the mrf entry writes");
    let app = common::test_app(pool);
    for path in ["/api/v1/instance", "/api/v2/instance"] {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(response.status, StatusCode::OK, "{path} answers");
        let federation = response.json_value()["pleroma"]["metadata"]["federation"].clone();
        assert_eq!(
            federation["mrf_policies"],
            json!(["NoOpPolicy"]),
            "{path}: the chain's names, the unknown one dropped"
        );
        assert_eq!(
            federation["mrf_noop"],
            json!({}),
            "{path}: the shipped policy describes itself empty"
        );
        assert_eq!(federation["enabled"], true, "{path}: still federating");
        assert_eq!(
            federation["exclusions"], false,
            "{path}: the empty transparency lists exclude nothing"
        );
    }
}

/// `MRF_POLICIES` enables the advertised chain with no database row: the
/// env fallback reaches the very same metadata block.
#[tokio::test]
async fn mrf_policies_env_enables_the_federation_block() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let mut config = common::test_config();
    config.mrf.policies = vec!["noop".to_owned()];
    let app = common::test_app_with_config(pool, config);
    let response = common::TestRequest::get("/api/v1/instance")
        .send(&app)
        .await;
    assert_eq!(response.status, StatusCode::OK, "the document answers");
    assert_eq!(
        response.json_value()["pleroma"]["metadata"]["federation"]["mrf_policies"],
        json!(["NoOpPolicy"]),
        "the env list builds the advertised chain"
    );
}

/// The eleven `mrf_simple` keys the block publishes: every list Pleroma
/// maps out of its `:mrf_simple` config, the empty ones as the empty array.
const SIMPLE_KEYS: [&str; 11] = [
    "accept",
    "reject",
    "reject_deletes",
    "federated_timeline_removal",
    "followers_only",
    "media_removal",
    "media_nsfw",
    "report_removal",
    "avatar_removal",
    "banner_removal",
    "background_removal",
];

/// The federation block publishes the `simple` lists whole: every list
/// under its own name in `mrf_simple`, the operators' reasons in the
/// separate `mrf_simple_info` object beside it, both on the two document
/// versions.
#[tokio::test]
async fn federation_publishes_the_simple_policy_lists() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let setting: Value = serde_json::from_str(SIMPLE_LISTS).expect("the fixture is JSON");
    set_mrf(&pool, &setting).await;
    let app = common::test_app(pool);
    for path in ["/api/v1/instance", "/api/v2/instance"] {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(response.status, StatusCode::OK, "{path} answers");
        let federation = response.json_value()["pleroma"]["metadata"]["federation"].clone();
        assert_eq!(
            federation["mrf_policies"],
            json!(["SimplePolicy"]),
            "{path}: the running policy, Pleroma-named"
        );
        assert_eq!(
            federation["mrf_simple"]["reject"],
            json!(["blocked.example", "*.worse.example"]),
            "{path}: the patterns as configured, wildcard verbatim"
        );
        assert_eq!(
            federation["mrf_simple"]["media_nsfw"],
            json!(["lewd.example"]),
            "{path}: every list that holds something is published"
        );
        assert_eq!(
            federation["mrf_simple"]["accept"],
            json!([]),
            "{path}: and the lists that hold nothing are published empty, \
             the way Pleroma maps its whole config"
        );
        for key in SIMPLE_KEYS {
            assert!(
                federation["mrf_simple"].get(key).is_some(),
                "{path}: `{key}` is one of the published lists"
            );
        }
        assert_eq!(
            federation["mrf_simple_info"],
            json!({
                "reject": {"blocked.example": {"reason": "coordinated abuse"}},
                "media_nsfw": {"lewd.example": {"reason": "no nsfw"}},
            }),
            "{path}: the reasons ride in the separate info object, for the \
             lists whose entries carry one"
        );
        assert_eq!(
            federation["quarantined_instances"],
            json!([]),
            "{path}: no instance is quarantined here"
        );
        assert!(
            !federation["exclusions"].as_bool().unwrap_or(true),
            "{path}: and nothing is held back"
        );
    }
}

/// `MRF_TRANSPARENCY=false` leaves the block the three fixed keys: no
/// policy names, no lists, no quarantine key — the moderation stays in
/// force, it is simply not described.
#[tokio::test]
async fn federation_publishes_nothing_when_transparency_is_off() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let setting: Value = serde_json::from_str(SIMPLE_LISTS).expect("the fixture is JSON");
    set_mrf(&pool, &setting).await;
    let mut config = common::test_config();
    config.mrf.transparency = Toggle::new(false);
    config.mrf.transparency_exclusions = vec!["press.example".to_owned()];
    let app = common::test_app_with_config(pool, config);
    for path in ["/api/v1/instance", "/api/v2/instance"] {
        let response = common::TestRequest::get(path).send(&app).await;
        assert_eq!(response.status, StatusCode::OK, "{path} answers");
        let document = response.json_value();
        assert_eq!(
            document["pleroma"]["metadata"]["federation"],
            json!({"enabled": true, "mrf_policies": [], "exclusions": false}),
            "{path}: the block is exactly the keys the switch leaves standing"
        );
        assert_eq!(
            document["description"],
            json!("A Wired instance, compatible with Pleroma clients"),
            "{path}: and the rest of the document is unaffected"
        );
    }
}

/// An excluded host is gone from both published objects — pattern and
/// reason — and the block discloses that its lists hold something back.
#[tokio::test]
async fn federation_exclusions_hide_hosts_and_are_disclosed() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let setting: Value = serde_json::from_str(SIMPLE_LISTS).expect("the fixture is JSON");
    set_mrf(&pool, &setting).await;
    let mut config = common::test_config();
    config.mrf.transparency_exclusions =
        vec!["*.worse.example".to_owned(), "lewd.example".to_owned()];
    let app = common::test_app_with_config(pool, config);
    for path in ["/api/v1/instance", "/api/v2/instance"] {
        let response = common::TestRequest::get(path).send(&app).await;
        let federation = response.json_value()["pleroma"]["metadata"]["federation"].clone();
        assert_eq!(
            federation["mrf_simple"]["reject"],
            json!(["blocked.example"]),
            "{path}: the excluded pattern is out of its list"
        );
        assert_eq!(
            federation["mrf_simple"]["media_nsfw"],
            json!([]),
            "{path}: a list emptied by exclusion stays a published, empty list"
        );
        assert_eq!(
            federation["mrf_simple_info"],
            json!({"reject": {"blocked.example": {"reason": "coordinated abuse"}}}),
            "{path}: and the excluded host's reason went with it"
        );
        assert_eq!(
            federation["mrf_policies"],
            json!(["SimplePolicy"]),
            "{path}: the policy is still named, and still running"
        );
        assert_eq!(
            federation["exclusions"], true,
            "{path}: using the exclusion list is itself disclosed"
        );
    }
}

/// A host the `simple` policy's `reject` list names leaves the peer
/// listing — the apex of a wildcard entry and everything below it — while
/// the rest of the mirrored world stays. The transparency settings do not
/// enter: a host the instance refuses is not one it advertises, whether or
/// not it publishes its lists.
#[tokio::test]
async fn peers_exclude_the_rejected_hosts() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    mirror_remote(&pool, "remote.example").await;
    mirror_remote(&pool, "evil.example").await;
    mirror_remote(&pool, "cdn.evil.example").await;
    mirror_remote(&pool, "quiet.example").await;
    let setting: Value = serde_json::from_str(
        r#"{"policies": ["simple"], "simple": {"reject": ["*.evil.example"]}}"#,
    )
    .expect("the fixture is JSON");
    set_mrf(&pool, &setting).await;

    for transparency in [true, false] {
        let mut config = common::test_config();
        config.mrf.transparency = Toggle::new(transparency);
        let app = common::test_app_with_config(pool.clone(), config);
        let peers: Vec<String> = common::TestRequest::get("/api/v1/instance/peers")
            .send(&app)
            .await
            .json();
        assert_eq!(
            peers,
            vec!["quiet.example".to_owned(), "remote.example".to_owned()],
            "MRF_TRANSPARENCY={transparency}: the rejected apex and every \
             host below it are out of the peer list"
        );
    }
}

/// A `simple` policy that is configured but not enabled hides no peer, and
/// an `accept` allowlist hides none either: the peer listing follows the
/// `reject` list alone.
#[tokio::test]
async fn peers_stay_listed_for_lists_that_are_not_rejections() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    mirror_remote(&pool, "peer.example").await;
    mirror_remote(&pool, "stranger.example").await;

    let off = json!({"policies": ["noop"], "simple": {"reject": ["peer.example"]}});
    set_mrf(&pool, &off).await;
    let disabled = common::test_app(pool.clone());
    let peers: Vec<String> = common::TestRequest::get("/api/v1/instance/peers")
        .send(&disabled)
        .await
        .json();
    assert_eq!(
        peers,
        vec!["peer.example".to_owned(), "stranger.example".to_owned()],
        "a list behind a switched-off policy governs nothing, the peer list included"
    );

    let allowlist = json!({"policies": ["simple"], "simple": {"accept": ["peer.example"]}});
    set_mrf(&pool, &allowlist).await;
    // A fresh app: the previous one has the previous setting cached.
    let allowlisted = common::test_app(pool);
    let still_listed: Vec<String> = common::TestRequest::get("/api/v1/instance/peers")
        .send(&allowlisted)
        .await
        .json();
    assert_eq!(
        still_listed,
        vec!["peer.example".to_owned(), "stranger.example".to_owned()],
        "and an allowlist refuses a host from federating without naming it \
         on the reject list"
    );
}
