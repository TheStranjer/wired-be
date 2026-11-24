//! Remote interaction over HTTP (WBE-188): the `POST
//! /api/v1/pleroma/remote_interaction` endpoint, the retired
//! `/ostatus_subscribe` pair (WBE-741: answered by the fallback alone),
//! `GET /authorize_interaction`'s frontend redirects and the retired
//! `/main/ostatus` pair, also answered by the fallback alone
//! (`src/api/remote_interaction.rs`).
//!
//! The WebFinger half of the flow needs a live `https://{domain}` peer,
//! so the HTTP success path is pinned by the module's own mock-server
//! unit tests; what this suite pins over the real router is the
//! parameter refusals (every case is network-free: an unresolvable
//! profile is spelled with a single-label domain that the `WebFinger`
//! client rejects before any request), and the redirects a locally
//! stored actor, handle and status get.

#![cfg(test)]

use axum::http::StatusCode;
use serde_json::json;
use wired_be::models::{follow, status::Visibility};

use crate::common;

/// The JSON endpoint under test.
const ROUTE: &str = "/api/v1/pleroma/remote_interaction";
/// The instance origin every fixture URL is built from.
const BASE: &str = "https://test.example";
/// A local actor URL, the `ap_id` the endpoint is asked to expand.
const AP_ID: &str = "https://test.example/users/alice";
/// A handle `WebFinger` refuses before any request (single-label
/// domain), so the refusal tests stay network-free.
const UNRESOLVABLE: &str = "ghost@invalid";
/// The 400 body both unresolvable-profile paths share.
const COULD_NOT_FIND_USER: &str = r#"{"error":"Couldn't find user"}"#;
/// The 404 body of `GET /authorize_interaction`.
const NOT_FOUND: &str = r#"{"error":"Record not found"}"#;

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

/// `GET /authorize_interaction?uri=<local actor url>` 302s the browser
/// to the frontend profile page, `{FRONTEND_BASE_URL}/users/{id}` — the
/// configured frontend origin, not `BASE_URL`, is what the redirect
/// names.
#[tokio::test]
async fn authorize_interaction_redirects_an_actor_url() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let mut config = common::test_config();
    config.frontend_base_url = "https://frontend.example".to_owned();
    let app = common::test_app_with_config(pool, config);

    let response = common::TestRequest::get(&format!(
        "/authorize_interaction?uri={}",
        urlencoding(AP_ID)
    ))
    .send(&app)
    .await;

    common::assert_answer(&response, StatusCode::FOUND, "the actor redirect");
    assert_eq!(
        response.header("location"),
        Some(format!("https://frontend.example/users/{}", alice.id).as_str()),
        "the frontend page for the resolved actor"
    );
}

/// A `user@domain` handle stored as a remote mirror resolves from the
/// database (no network), with its `@` prefix folded.
#[tokio::test]
async fn authorize_interaction_redirects_a_handle() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let bob = common::create_remote_user(&pool, "bob", "remote.example").await;
    let app = common::test_app(pool);

    let response = common::TestRequest::get(&format!(
        "/authorize_interaction?uri={}",
        urlencoding("@bob@remote.example")
    ))
    .send(&app)
    .await;

    common::assert_answer(&response, StatusCode::FOUND, "the handle redirect");
    assert_eq!(
        response.header("location"),
        Some(format!("{BASE}/users/{}", bob.id).as_str()),
        "the default frontend origin is the instance origin"
    );
}

/// A status URL resolves through the object reader and 302s to
/// `{FRONTEND_BASE_URL}/notice/{id}`.
#[tokio::test]
async fn authorize_interaction_redirects_a_status_url() {
    let db = common::test_db().await;
    let pool = db.pool().clone();
    let alice = common::create_local_user(&pool, "alice").await;
    let note = common::create_status(&pool, &alice, "an interacted note", Visibility::Public).await;
    let app = common::test_app(pool);

    let response = common::TestRequest::get(&format!(
        "/authorize_interaction?uri={}",
        urlencoding(&note.uri)
    ))
    .send(&app)
    .await;

    common::assert_answer(&response, StatusCode::FOUND, "the status redirect");
    assert_eq!(
        response.header("location"),
        Some(format!("{BASE}/notice/{}", note.id).as_str()),
        "the frontend page for the resolved status"
    );
}

/// Whatever the `uri` cannot resolve to — a handle that fails the
/// grammar, an unknown actor URL on this instance's own origin, or no
/// `uri` at all — is the 404 JSON. None of these reach the network.
#[tokio::test]
async fn authorize_interaction_refuses_unresolvable_uris() {
    let db = common::test_db().await;
    let app = common::test_app(db.pool().clone());

    let handle = common::TestRequest::get(&format!(
        "/authorize_interaction?uri={}",
        urlencoding(UNRESOLVABLE)
    ))
    .send(&app)
    .await;
    let unknown_actor = common::TestRequest::get(&format!(
        "/authorize_interaction?uri={}",
        urlencoding("https://test.example/users/nobody")
    ))
    .send(&app)
    .await;
    let missing = common::TestRequest::get("/authorize_interaction")
        .send(&app)
        .await;
    for response in [&handle, &unknown_actor, &missing] {
        common::assert_answer(response, StatusCode::NOT_FOUND, "an unresolvable uri");
        assert_eq!(
            response.text(),
            NOT_FOUND,
            "the 404 is JSON: {}",
            response.text()
        );
    }
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
