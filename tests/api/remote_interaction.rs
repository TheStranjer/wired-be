//! Remote interaction over HTTP (WBE-188): the `POST
//! /api/v1/pleroma/remote_interaction` endpoint, the retired
//! `/ostatus_subscribe` pair (WBE-741: answered by the fallback alone),
//! the retired `GET /authorize_interaction` (WBE-752: the frontend serves
//! it, the fallback answers here and nothing is fetched) and the retired
//! `/main/ostatus` pair, also answered by the fallback alone
//! (`src/api/remote_interaction.rs`).
//!
//! The WebFinger half of the flow needs a live `https://{domain}` peer,
//! so the HTTP success path is pinned by the module's own mock-server
//! unit tests; what this suite pins over the real router is the
//! parameter refusals (every case is network-free: an unresolvable
//! profile is spelled with a single-label domain that the `WebFinger`
//! client rejects before any request).

#![cfg(test)]

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::follow;

use crate::common;

/// The JSON endpoint under test.
const ROUTE: &str = "/api/v1/pleroma/remote_interaction";
/// A local actor URL, the `ap_id` the endpoint is asked to expand.
const AP_ID: &str = "https://test.example/users/alice";
/// A handle `WebFinger` refuses before any request (single-label
/// domain), so the refusal tests stay network-free.
const UNRESOLVABLE: &str = "ghost@invalid";
/// The 400 body both unresolvable-profile paths share.
const COULD_NOT_FIND_USER: &str = r#"{"error":"Couldn't find user"}"#;

/// The 400 an absent, blank or `null` `ap_id` is refused with.
#[tokio::test]
async fn remote_interaction_refuses_a_missing_ap_id() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let no_field = common::TestRequest::post(ROUTE)
        .form(&[("profile", "bob@remote.example")])
        .send(&app)
        .await;
    let blank = common::TestRequest::post(ROUTE)
        .form(&[("ap_id", "   "), ("profile", "bob@remote.example")])
        .send(&app)
        .await;
    let nulled = common::TestRequest::post(ROUTE)
        .json(&json!({"ap_id": null, "profile": "bob@remote.example"}))
        .send(&app)
        .await;
    let nothing = common::TestRequest::post(ROUTE).send(&app).await;
    for response in [&no_field, &blank, &nulled, &nothing] {
        common::assert_answer(response, StatusCode::BAD_REQUEST, "a missing ap_id");
        common::assert_error(response, "Missing parameter: ap_id", "a missing ap_id");
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "refusals are JSON (decision 8)"
        );
    }
}

/// The 400 an absent, blank or `null` `profile` is refused with — only
/// once `ap_id` is in, since the checks run in parameter order.
#[tokio::test]
async fn remote_interaction_refuses_a_missing_profile() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let no_field = common::TestRequest::post(ROUTE)
        .form(&[("ap_id", AP_ID)])
        .send(&app)
        .await;
    let blank = common::TestRequest::post(ROUTE)
        .form(&[("ap_id", AP_ID), ("profile", "")])
        .send(&app)
        .await;
    let nulled = common::TestRequest::post(ROUTE)
        .json(&json!({"ap_id": AP_ID, "profile": null}))
        .send(&app)
        .await;
    for response in [&no_field, &blank, &nulled] {
        common::assert_answer(response, StatusCode::BAD_REQUEST, "a missing profile");
        common::assert_error(response, "Missing parameter: profile", "a missing profile");
    }
}

/// A profile this instance cannot resolve — an unusable handle grammar,
/// so no `WebFinger` request ever leaves — is the 400
/// `{"error":"Couldn't find user"}`, from a form and from JSON alike.
#[tokio::test]
async fn remote_interaction_refuses_an_unresolvable_profile() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let form = common::TestRequest::post(ROUTE)
        .form(&[("ap_id", AP_ID), ("profile", UNRESOLVABLE)])
        .send(&app)
        .await;
    let json_body = common::TestRequest::post(ROUTE)
        .json(&json!({"ap_id": AP_ID, "profile": "nobody"}))
        .send(&app)
        .await;
    for response in [&form, &json_body] {
        common::assert_answer(response, StatusCode::BAD_REQUEST, "an unresolvable profile");
        assert_eq!(
            response.text(),
            COULD_NOT_FIND_USER,
            "the body Pleroma's controller answers: {}",
            response.text()
        );
    }
}

/// The anonymous endpoint counts into the `remote_interaction` bucket
/// (WBE-814): the limited config decorates answers with `x-ratelimit-*`
/// headers and refuses over the budget with 429, per client address.
#[tokio::test]
async fn remote_interaction_is_throttled_per_client() {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    let db = common::test_db().await;
    let config = common::test_config_with_rate_limits("remote_interaction=2:600");
    let app = common::test_app_with_config(db.pool().clone(), config);
    let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9)), 40_009);
    let other = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)), 40_010);

    let first = common::TestRequest::post(ROUTE)
        .form(&[("ap_id", AP_ID), ("profile", UNRESOLVABLE)])
        .peer(peer)
        .send(&app)
        .await;
    common::assert_answer(&first, StatusCode::BAD_REQUEST, "the counted request");
    assert_eq!(first.header("x-ratelimit-limit"), Some("2"), "the budget");
    assert_eq!(
        first.header("x-ratelimit-remaining"),
        Some("1"),
        "one request spent of two"
    );
    let _second = common::TestRequest::post(ROUTE)
        .form(&[("ap_id", AP_ID), ("profile", UNRESOLVABLE)])
        .peer(peer)
        .send(&app)
        .await;
    let third = common::TestRequest::post(ROUTE)
        .form(&[("ap_id", AP_ID), ("profile", UNRESOLVABLE)])
        .peer(peer)
        .send(&app)
        .await;
    common::assert_answer(&third, StatusCode::TOO_MANY_REQUESTS, "over the budget");
    common::assert_error(&third, "Throttled", "the throttle body");

    let elsewhere = common::TestRequest::post(ROUTE)
        .form(&[("ap_id", AP_ID), ("profile", UNRESOLVABLE)])
        .peer(other)
        .send(&app)
        .await;
    common::assert_answer(
        &elsewhere,
        StatusCode::BAD_REQUEST,
        "another client has its own window",
    );
}

/// A `profile` naming a non-standard port is refused as an unknown
/// profile by default (WBE-814), before any lookup is made.
#[tokio::test]
async fn remote_interaction_refuses_a_handle_naming_a_non_standard_port() {
    let db = common::test_db().await;
    let config = common::test_config_with_rate_limits("remote_interaction=100:60");
    let app = common::test_app_with_config(db.pool().clone(), config);

    for profile in ["bob@remote.example:8080", "bob@203.0.113.9:6379"] {
        let response = common::TestRequest::post(ROUTE)
            .json(&json!({"ap_id": AP_ID, "profile": profile}))
            .send(&app)
            .await;
        common::assert_answer(&response, StatusCode::BAD_REQUEST, "a ported handle");
        assert_eq!(
            response.text(),
            COULD_NOT_FIND_USER,
            "{profile}: refused like an unknown profile"
        );
    }
}

/// `POST /ostatus_subscribe` is not routed (WBE-741): even a local user's
/// correct password follows nobody — the fallback answers 404 and no
/// `follows` row is written, so no unauthenticated route takes a password.
#[tokio::test]
async fn the_subscribe_post_is_answered_by_the_fallback_and_follows_nobody() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_user_with_password(&pool, "alice", "s3cret-hunter").await;
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let app = common::test_app(pool.clone());

    let response = common::TestRequest::post("/ostatus_subscribe")
        .form(&[
            ("acct", "bob@remote.example"),
            ("authorization[name]", "alice"),
            ("authorization[password]", "s3cret-hunter"),
        ])
        .send(&app)
        .await;

    common::assert_answer(
        &response,
        StatusCode::NOT_FOUND,
        "the retired subscribe post",
    );
    assert_eq!(
        response.header("x-wired-fallback"),
        Some("1"),
        "only the fallback answers the path"
    );
    let relation = follow::get(&pool, alice.id, bob.id)
        .await
        .expect("the follow read");
    assert!(relation.is_none(), "a correct password follows nobody");
}

/// `GET /ostatus_subscribe` is not routed either: the login form is gone.
#[tokio::test]
async fn the_subscribe_form_is_answered_by_the_fallback() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let response = common::TestRequest::get("/ostatus_subscribe?acct=x")
        .send(&app)
        .await;

    common::assert_answer(
        &response,
        StatusCode::NOT_FOUND,
        "the retired subscribe form",
    );
    assert_eq!(
        response.header("x-wired-fallback"),
        Some("1"),
        "only the fallback answers the path"
    );
}

/// `GET /authorize_interaction` is not routed (the frontend serves it):
/// the fallback answers 404, and an anonymous caller cannot make the
/// server fetch or store the remote actor the `uri` names.
#[tokio::test]
async fn authorize_interaction_is_answered_by_the_fallback_and_fetches_nothing() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let app = common::test_app(pool.clone());

    let remote_actor = "https://remote.example/users/x";
    for uri in [remote_actor, "@x@remote.example", ""] {
        let response =
            common::TestRequest::get(&format!("/authorize_interaction?uri={}", urlencoding(uri)))
                .send(&app)
                .await;
        common::assert_answer(&response, StatusCode::NOT_FOUND, "the retired page");
        assert_eq!(
            response.header("x-wired-fallback"),
            Some("1"),
            "only the fallback answers the path"
        );
    }

    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE ap_id = $1")
        .bind(remote_actor)
        .fetch_one(&pool)
        .await
        .expect("the users count");
    assert_eq!(stored, 0, "no actor row was created for the uri");
}

/// `GET /main/ostatus` and `POST /main/ostatus` are not routed: the
/// server-rendered remote-subscribe form is gone, and only the fallback
/// answers both methods.
#[tokio::test]
async fn the_main_ostatus_pair_is_answered_by_the_fallback() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let get = common::TestRequest::get("/main/ostatus?nickname=alice&profile=bob%40remote.example")
        .send(&app)
        .await;
    let post = common::TestRequest::post("/main/ostatus")
        .form(&[
            ("user[nickname]", "alice"),
            ("user[profile]", "bob@remote.example"),
        ])
        .send(&app)
        .await;

    for (response, who) in [(&get, "the retired form"), (&post, "the retired post")] {
        common::assert_answer(response, StatusCode::NOT_FOUND, who);
        assert_eq!(
            response.header("x-wired-fallback"),
            Some("1"),
            "only the fallback answers the path"
        );
    }
}

/// The `path` as a percent-encoded query value, so fixture URLs and
/// handles survive the query string intact.
fn urlencoding(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}
