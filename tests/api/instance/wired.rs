//! The `wired` extension block of both instance documents (decision 17).

use serde_json::{Value, json};
use wired_be::config::{
    BASE_URL_VAR, Config, DATABASE_URL_VAR, PROTOCOLS_VAR, TEST_BASE_URL, TEST_DATABASE_URL,
};

use super::support::get_instance;
use crate::common;

/// The test configuration with `PROTOCOLS` set to `protocols`.
fn config_with_protocols(protocols: &str) -> Config {
    Config::from_lookup(|key| match key {
        DATABASE_URL_VAR => Some(TEST_DATABASE_URL.to_owned()),
        BASE_URL_VAR => Some(TEST_BASE_URL.to_owned()),
        PROTOCOLS_VAR => Some(protocols.to_owned()),
        _ => None,
    })
    .expect("the fixed protocols test config is valid")
}

/// `GET /api/v2/instance` and require a 200 JSON document.
async fn get_instance_v2(app: &axum::Router) -> Value {
    let response = common::TestRequest::get("/api/v2/instance").send(app).await;
    assert_eq!(
        response.status,
        axum::http::StatusCode::OK,
        "the v2 instance document is served"
    );
    response.json_value()
}

/// With the default `PROTOCOLS`, both documents list `ActivityPub` alone and
/// advertise the `wired:protocols` flag.
#[tokio::test]
async fn the_default_config_lists_activitypub_only() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());
    for document in [get_instance(&app).await, get_instance_v2(&app).await] {
        assert_eq!(document["wired"], json!({"protocols": ["activitypub"]}));
        let features = document["pleroma"]["metadata"]["features"]
            .as_array()
            .expect("the feature list is an array");
        assert!(
            features.contains(&json!("wired:protocols")),
            "the extension is advertised"
        );
    }
}

/// With `PROTOCOLS=activitypub,fixture`, both documents list both protocols
/// in registry order.
#[tokio::test]
async fn every_enabled_protocol_is_listed() {
    let db = common::test_db().await;
    let config = config_with_protocols("fixture,activitypub");
    let app = common::test_app_with_config(db.pool().clone(), config);
    for document in [get_instance(&app).await, get_instance_v2(&app).await] {
        assert_eq!(
            document["wired"],
            json!({"protocols": ["activitypub", "fixture"]})
        );
    }
}
